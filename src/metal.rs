use crate::hash::{Midstate, Target};
use anyhow::{bail, Result};

pub const HITS: usize = 256;
pub const SLOTS: usize = 2;

pub struct Scan {
    pub count: u32,
    pub full: u32,
    pub nonces: Vec<u32>,
    pub gpu_seconds: f64,
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::hash::K;
    use anyhow::ensure;
    use std::{
        cell::Cell,
        ffi::{c_char, c_void, CStr, CString},
        ptr::NonNull,
    };

    extern "C" {
        fn silicon_metal_new(
            source: *const c_char,
            slots: usize,
            output_size: usize,
            requested_width: usize,
            actual_width: *mut usize,
            message: *mut c_char,
            len: usize,
        ) -> *mut c_void;
        fn silicon_metal_submit(
            context: *mut c_void,
            slot: usize,
            job: *const Job,
            job_size: usize,
            count: u32,
            digest: bool,
            message: *mut c_char,
            len: usize,
        ) -> bool;
        fn silicon_metal_wait(
            context: *mut c_void,
            slot: usize,
            output: *mut u32,
            words: usize,
            gpu_seconds: *mut f64,
            message: *mut c_char,
            len: usize,
        ) -> bool;
        fn silicon_metal_discard(context: *mut c_void, slot: usize);
        fn silicon_metal_release(context: *mut c_void);
    }

    #[repr(C)]
    struct Job {
        mid: [u32; 8],
        pre: [u32; 8],
        tail: [u32; 3],
        w16: u32,
        w17: u32,
        target: [u32; 8],
        start: u32,
        count: u32,
    }

    impl Job {
        fn new(mid: &Midstate, target: Target, start: u32, count: u32) -> Self {
            let tail = mid.tail_words();
            let mut pre = mid.words;
            for (word, k) in tail.into_iter().zip(K) {
                let [a, b, c, d, e, f, g, h] = pre;
                let big0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let big1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let t1 = h
                    .wrapping_add(big1)
                    .wrapping_add(g ^ (e & (f ^ g)))
                    .wrapping_add(k)
                    .wrapping_add(word);
                let t2 = big0.wrapping_add((a & b) | (c & (a | b)));
                pre = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
            }
            let sigma0 = |x: u32| x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let sigma1 = |x: u32| x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
            Self {
                mid: mid.words,
                pre,
                tail,
                w16: tail[0].wrapping_add(sigma0(tail[1])),
                w17: tail[1]
                    .wrapping_add(sigma0(tail[2]))
                    .wrapping_add(sigma1(640)),
                target: std::array::from_fn(|i| {
                    u32::from_be_bytes(target.0[i * 4..i * 4 + 4].try_into().unwrap())
                }),
                start,
                count,
            }
        }
    }

    pub struct Gpu {
        handle: NonNull<c_void>,
        busy: [Cell<bool>; SLOTS],
        pub name: String,
        pub width: usize,
    }

    pub struct Batch<'a> {
        gpu: &'a Gpu,
        slot: usize,
        count: u32,
    }

    fn diagnostic(message: &[c_char]) -> String {
        // The buffer is pre-zeroed and the bridge writes via snprintf.
        unsafe { CStr::from_ptr(message.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    impl Gpu {
        pub fn new() -> Result<Self> {
            let requested = match std::env::var("SILICON_METAL_GROUP") {
                Ok(value) => {
                    let width = value.parse::<usize>()?;
                    ensure!(
                        (1..=1024).contains(&width),
                        "Metal threadgroup size is out of range"
                    );
                    width
                }
                Err(std::env::VarError::NotPresent) => 0,
                Err(error) => return Err(error.into()),
            };
            let source = CString::new(include_str!("../native/sha256.metal"))?;
            let mut message = [0; 2048];
            let mut width = 0;
            let handle = unsafe {
                silicon_metal_new(
                    source.as_ptr(),
                    SLOTS,
                    (HITS + 2) * 4,
                    requested,
                    &mut width,
                    message.as_mut_ptr(),
                    message.len(),
                )
            };
            let name = diagnostic(&message);
            let Some(handle) = NonNull::new(handle) else {
                bail!("{name} Select CPU mode.")
            };
            Ok(Self {
                handle,
                busy: Default::default(),
                name,
                width,
            })
        }

        fn submit_inner(&self, job: Job, digest: bool) -> Result<Batch<'_>> {
            ensure!(
                job.count > 0 && u64::from(job.start) + u64::from(job.count) <= 1u64 << 32,
                "Invalid Metal nonce range"
            );
            let slot = self
                .busy
                .iter()
                .position(|busy| !busy.get())
                .ok_or_else(|| anyhow::anyhow!("No free Metal slot"))?;
            let mut message = [0; 2048];
            let ok = unsafe {
                silicon_metal_submit(
                    self.handle.as_ptr(),
                    slot,
                    &job,
                    std::mem::size_of::<Job>(),
                    job.count,
                    digest,
                    message.as_mut_ptr(),
                    message.len(),
                )
            };
            ensure!(ok, "{}", diagnostic(&message));
            self.busy[slot].set(true);
            Ok(Batch {
                gpu: self,
                slot,
                count: job.count,
            })
        }

        pub fn submit(
            &self,
            mid: &Midstate,
            target: Target,
            start: u32,
            count: u32,
        ) -> Result<Batch<'_>> {
            self.submit_inner(Job::new(mid, target, start, count), false)
        }

        pub fn digest(&self, mid: &Midstate, nonce: u32) -> Result<[u8; 32]> {
            let mut words = [0u32; 8];
            self.submit_inner(Job::new(mid, Target([0; 32]), nonce, 1), true)?
                .read(&mut words)?;
            let mut digest = [0; 32];
            for (word, chunk) in words.into_iter().zip(digest.chunks_exact_mut(4)) {
                chunk.copy_from_slice(&word.to_be_bytes());
            }
            Ok(digest)
        }
    }

    impl Drop for Gpu {
        fn drop(&mut self) {
            unsafe { silicon_metal_release(self.handle.as_ptr()) }
        }
    }

    impl Batch<'_> {
        fn read(self, output: &mut [u32]) -> Result<f64> {
            let mut message = [0; 2048];
            let mut seconds = 0.0;
            let ok = unsafe {
                silicon_metal_wait(
                    self.gpu.handle.as_ptr(),
                    self.slot,
                    output.as_mut_ptr(),
                    output.len(),
                    &mut seconds,
                    message.as_mut_ptr(),
                    message.len(),
                )
            };
            self.gpu.busy[self.slot].set(false);
            ensure!(ok, "{}", diagnostic(&message));
            Ok(seconds)
        }

        pub fn hits(self) -> Result<Scan> {
            let mut output = [0u32; HITS + 2];
            let nonces = self.count;
            let gpu_seconds = self.read(&mut output)?;
            let [count, full, ..] = output;
            ensure!(
                count <= nonces && full <= nonces,
                "Metal counters exceed the batch size"
            );
            Ok(Scan {
                count,
                full,
                gpu_seconds,
                nonces: output[2..2 + (count as usize).min(HITS)].to_vec(),
            })
        }
    }

    impl Drop for Batch<'_> {
        fn drop(&mut self) {
            // On an early exit, drain the command before the buffer can be reused.
            if self.gpu.busy[self.slot].replace(false) {
                unsafe { silicon_metal_discard(self.gpu.handle.as_ptr(), self.slot) }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub struct Gpu {
        pub name: String,
        pub width: usize,
    }
    pub struct Batch<'a>(std::marker::PhantomData<&'a Gpu>);

    impl Gpu {
        pub fn new() -> Result<Self> {
            bail!("Metal is unavailable on this platform. Select CPU mode.")
        }
        pub fn submit(&self, _: &Midstate, _: Target, _: u32, _: u32) -> Result<Batch<'_>> {
            Self::new()?;
            unreachable!()
        }
        pub fn digest(&self, _: &Midstate, _: u32) -> Result<[u8; 32]> {
            Self::new()?;
            unreachable!()
        }
    }

    impl Batch<'_> {
        pub fn hits(self) -> Result<Scan> {
            unreachable!()
        }
    }
}

pub use platform::{Batch, Gpu};

pub fn verify(gpu: &Gpu) -> Result<()> {
    use crate::hash::{decode, display_hash, sha256d_bytes, with_nonce, GENESIS, GENESIS_ID};
    use anyhow::ensure;

    let genesis = decode(GENESIS)?;
    let genesis_nonce = u32::from_le_bytes(genesis[76..].try_into()?);
    for header in [
        genesis,
        [0; 80],
        [255; 80],
        std::array::from_fn(|i| (i * 31 + 7) as u8),
    ] {
        let mid = Midstate::new(&header);
        for nonce in [0, 0x01020304, genesis_nonce, u32::MAX - 1, u32::MAX] {
            let reference = sha256d_bytes(&with_nonce(header, nonce));
            ensure!(
                mid.hash(nonce) == reference && gpu.digest(&mid, nonce)? == reference,
                "CPU and Metal results differ at nonce {nonce:08x}"
            );
            let mut exact = reference;
            exact.reverse();
            ensure!(
                gpu.submit(&mid, Target(exact), nonce, 1)?.hits()?.nonces == [nonce],
                "Metal rejected a hash equal to the target"
            );
            // Change only the low word so both comparisons take the equality path for H7.
            let mut harder = exact;
            if let Some(last) = harder.last_mut().filter(|byte| **byte > 0) {
                *last -= 1;
                let scan = gpu.submit(&mid, Target(harder), nonce, 1)?.hits()?;
                ensure!(
                    scan.count == 0 && scan.full == 1,
                    "Metal handled an H7 match incorrectly"
                );
            }
        }
        let target = Target::compact(0x207fffff)?;
        let start = u32::MAX - 180;
        let expected: Vec<_> = (start..=u32::MAX)
            .filter(|&n| target.meets(&sha256d_bytes(&with_nonce(header, n))))
            .collect();
        // Read the two slots in reverse order, then reuse them in subsequent checks.
        let first = gpu.submit(&mid, target, start, 181)?;
        let second = gpu.submit(&mid, Target([255; 32]), 0, HITS as u32 + 3)?;
        let overflow = second.hits()?;
        ensure!(
            overflow.count as usize == HITS + 3 && overflow.nonces.len() == HITS,
            "Metal did not detect result buffer saturation correctly"
        );
        let mut scan = first.hits()?;
        scan.nonces.sort_unstable();
        ensure!(
            scan.nonces == expected,
            "Metal result mismatch for an unaligned range"
        );
    }
    let mid = Midstate::new(&genesis);
    ensure!(
        display_hash(gpu.digest(&mid, genesis_nonce)?) == GENESIS_ID,
        "Incorrect genesis block hash"
    );
    let scan = gpu
        .submit(&mid, Target::compact(0x1d00ffff)?, genesis_nonce - 1, 3)?
        .hits()?;
    ensure!(
        scan.nonces == [genesis_nonce],
        "Incorrect genesis block network target on Metal"
    );
    Ok(())
}

pub fn thermal_state() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn silicon_thermal_state() -> std::ffi::c_long;
        }
        match unsafe { silicon_thermal_state() } {
            0 => Some("nominal"),
            1 => Some("fair"),
            2 => Some("serious"),
            3 => Some("critical"),
            _ => None,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
