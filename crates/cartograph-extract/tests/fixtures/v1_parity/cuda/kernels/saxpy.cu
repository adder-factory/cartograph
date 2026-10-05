#include <cstdio>
#include <cuda_runtime.h>
#include "../include/vec_math.cuh"

__constant__ float kScale = 2.0f;
__device__ int g_hits;

__host__ __device__ float clampf(float v, float lo, float hi) {
    return v < lo ? lo : (v > hi ? hi : v);
}

__global__ void saxpyKernel(int n, float a, const float *x, float *y) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        y[i] = clampf(a * x[i] + y[i], 0.0f, kScale);
        atomicAdd(&g_hits, 1);
    }
}

__global__ void normKernel(const float3_t *in, float *out, int n) {
    __shared__ float tile[BLOCK_SIZE];
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    tile[threadIdx.x] = i < n ? dot3(in[i], in[i]) : 0.0f;
    __syncthreads();
    out[i] = tile[threadIdx.x];
}

static void checkError(cudaError_t err) {
    if (err != cudaSuccess) {
        printf("cuda error %s\n", cudaGetErrorString(err));
    }
}

void launchSaxpy(int n, float a, const float *x, float *y) {
    int blocks = (n + BLOCK_SIZE - 1) / BLOCK_SIZE;
    saxpyKernel<<<blocks, BLOCK_SIZE>>>(n, a, x, y);
    checkError(cudaGetLastError());
    cudaDeviceSynchronize();
}
