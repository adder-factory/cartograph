#ifndef COMMON_HLSLI
#define COMMON_HLSLI

#define PI 3.14159265

struct VSInput {
  float3 pos : POSITION;
  float3 normal : NORMAL;
  float2 uv : TEXCOORD0;
};

struct PSInput {
  float4 pos : SV_Position;
  float3 normal : NORMAL;
  float2 uv : TEXCOORD0;
};

cbuffer PerFrame : register(b0) {
  float4x4 viewProj;
  float3 lightDir;
};

float saturateDot(float3 a, float3 b) {
  return saturate(dot(a, b));
}

float3 lambert(float3 n, float3 l, float3 albedo) {
  return albedo * saturateDot(normalize(n), -l) / PI;
}

#endif
