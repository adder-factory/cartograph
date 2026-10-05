#ifndef VEC_MATH_CUH
#define VEC_MATH_CUH

#include <cuda_runtime.h>

#define BLOCK_SIZE 256
#define USE_FAST_MATH

typedef struct {
    float x;
    float y;
    float z;
} float3_t;

enum MemKind {
    MEM_HOST,
    MEM_DEVICE
};

__device__ __forceinline__ float dot3(const float3_t a, const float3_t b) {
    return a.x * b.x + a.y * b.y + a.z * b.z;
}

__host__ __device__ float clampf(float v, float lo, float hi);

#endif
