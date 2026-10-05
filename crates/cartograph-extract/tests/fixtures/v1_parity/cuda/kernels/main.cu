#include <cstdlib>
#include "../include/vec_math.cuh"

void launchSaxpy(int n, float a, const float *x, float *y);

template <typename T>
__global__ void fillKernel(T *out, T value, int n) {
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < n) out[idx] = value;
}

int main() {
    const int n = 1024;
    float *x = nullptr;
    float *y = nullptr;
    cudaMalloc(&x, n * sizeof(float));
    cudaMalloc(&y, n * sizeof(float));
    fillKernel<float><<<4, BLOCK_SIZE>>>(x, 1.0f, n);
    launchSaxpy(n, 2.0f, x, y);
    cudaFree(x);
    cudaFree(y);
    return 0;
}
