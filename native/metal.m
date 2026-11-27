#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

@interface SiliconSlot : NSObject
@property(strong) id<MTLCommandBuffer> command;
@property(strong) id<MTLBuffer> output;
@end
@implementation SiliconSlot
@end

@interface SiliconDevice : NSObject
@property(strong) id<MTLCommandQueue> queue;
@property(strong) id<MTLComputePipelineState> search;
@property(strong) id<MTLComputePipelineState> digest;
@property(strong) NSArray<SiliconSlot *> *slots;
@property NSUInteger width;
@end
@implementation SiliconDevice
@end

static void report(char *out, size_t length, NSString *message) {
    snprintf(out, length, "%s", message.UTF8String ?: "Metal error");
}

void *silicon_metal_new(
    const char *source,
    size_t slots,
    size_t output_size,
    size_t requested_width,
    size_t *actual_width,
    char *message,
    size_t length
) {
    @autoreleasepool {
        SiliconDevice *ctx = [SiliconDevice new];
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        if (!device) {
            report(message, length, @"No Metal device detected.");
            return NULL;
        }
        NSError *error = nil;
        NSString *code = [NSString stringWithUTF8String:source];
        id<MTLLibrary> library = [device newLibraryWithSource:code options:nil error:&error];
        if (!library) {
            report(message, length, error.localizedDescription);
            return NULL;
        }
        ctx.queue = [device newCommandQueue];
        id<MTLFunction> search = [library newFunctionWithName:@"search"];
        id<MTLFunction> digest = [library newFunctionWithName:@"vector_digest"];
        if (search && digest) {
            // The search dispatch always uses whole threadgroups. The shader
            // guards padded threads when the nonce range is not aligned.
            MTLComputePipelineDescriptor *descriptor = [MTLComputePipelineDescriptor new];
            descriptor.computeFunction = search;
            descriptor.threadGroupSizeIsMultipleOfThreadExecutionWidth = YES;
            ctx.search = [device newComputePipelineStateWithDescriptor:descriptor
                            options:MTLPipelineOptionNone reflection:NULL error:&error];
            if (ctx.search) ctx.digest = [device newComputePipelineStateWithFunction:digest error:&error];
        }
        if (!ctx.queue || !ctx.search || !ctx.digest) {
            report(message, length, error.localizedDescription ?: @"Could not initialize the Metal command queue.");
            return NULL;
        }
        NSUInteger simd = ctx.search.threadExecutionWidth;
        NSUInteger max = ctx.search.maxTotalThreadsPerThreadgroup;
        NSUInteger groups = [device.name isEqualToString:@"Apple M5 Pro"] ? 32 : 4;
        ctx.width = requested_width ?: MIN(max / simd, groups) * simd;
        if (!ctx.width || ctx.width > max || ctx.width % simd) {
            report(message, length, [NSString stringWithFormat:
                @"Metal threadgroup size must be a multiple of %lu and at most %lu.",
                (unsigned long)simd, (unsigned long)max]);
            return NULL;
        }
        NSMutableArray<SiliconSlot *> *buffers = [NSMutableArray arrayWithCapacity:slots];
        for (size_t i = 0; i < slots; ++i) {
            SiliconSlot *slot = [SiliconSlot new];
            slot.output = [device newBufferWithLength:output_size options:MTLResourceStorageModeShared];
            if (!slot.output) {
                report(message, length, @"Could not allocate a Metal result buffer.");
                return NULL;
            }
            [buffers addObject:slot];
        }
        ctx.slots = buffers;
        *actual_width = ctx.width;
        report(message, length, device.name);
        return (__bridge_retained void *)ctx;
    }
}

bool silicon_metal_submit(
    void *context,
    size_t index,
    const void *job,
    size_t job_size,
    uint32_t count,
    bool digest,
    char *message,
    size_t length
) {
    @autoreleasepool {
        SiliconDevice *ctx = (__bridge SiliconDevice *)context;
        SiliconSlot *slot = ctx.slots[index];
        id<MTLCommandBuffer> command = [ctx.queue commandBuffer];
        id<MTLComputeCommandEncoder> encoder = [command computeCommandEncoder];
        if (slot.command || !encoder) {
            report(message, length, @"Could not encode the Metal command.");
            return false;
        }
        if (!digest) memset(slot.output.contents, 0, 2 * sizeof(uint32_t));
        [encoder setComputePipelineState:digest ? ctx.digest : ctx.search];
        [encoder setBytes:job length:job_size atIndex:0];
        [encoder setBuffer:slot.output offset:0 atIndex:1];
        if (digest) {
            [encoder dispatchThreads:MTLSizeMake(1, 1, 1) threadsPerThreadgroup:MTLSizeMake(1, 1, 1)];
        } else {
            NSUInteger width = ctx.width;
            [encoder dispatchThreadgroups:MTLSizeMake(((NSUInteger)count + width - 1) / width, 1, 1)
                threadsPerThreadgroup:MTLSizeMake(width, 1, 1)];
        }
        [encoder endEncoding];
        slot.command = command;
        [command commit];
        return true;
    }
}

bool silicon_metal_wait(
    void *context,
    size_t index,
    uint32_t *out,
    size_t words,
    double *gpu_seconds,
    char *message,
    size_t length
) {
    @autoreleasepool {
        SiliconDevice *ctx = (__bridge SiliconDevice *)context;
        SiliconSlot *slot = ctx.slots[index];
        id<MTLCommandBuffer> command = slot.command;
        [command waitUntilCompleted];
        slot.command = nil;
        if (command.status != MTLCommandBufferStatusCompleted) {
            report(message, length, command.error.localizedDescription ?: @"Metal batch failed.");
            return false;
        }
        *gpu_seconds = command.GPUEndTime - command.GPUStartTime;
        const uint32_t *source = slot.output.contents;
        // A digest always writes eight words. Search only writes its two
        // counters and the occupied nonce entries.
        size_t used = words == 8 ? words : 2 + MIN((size_t)source[0], words - 2);
        memcpy(out, source, used * sizeof(uint32_t));
        return true;
    }
}

void silicon_metal_discard(void *context, size_t index) {
    @autoreleasepool {
        SiliconDevice *ctx = (__bridge SiliconDevice *)context;
        SiliconSlot *slot = ctx.slots[index];
        [slot.command waitUntilCompleted];
        slot.command = nil;
    }
}

void silicon_metal_release(void *handle) {
    @autoreleasepool {
        (void)CFBridgingRelease(handle);
    }
}

long silicon_thermal_state(void) {
    return (long)NSProcessInfo.processInfo.thermalState;
}
