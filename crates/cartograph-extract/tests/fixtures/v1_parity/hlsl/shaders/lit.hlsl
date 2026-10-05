#include "common.hlsli"

Texture2D albedoTex : register(t0);
SamplerState linearSampler : register(s0);

float helper(float x) {
  return x;
}

PSInput VSMain(VSInput input) {
  PSInput o;
  o.pos = mul(viewProj, float4(input.pos, 1.0));
  o.normal = input.normal;
  o.uv = input.uv;
  return o;
}

float4 PSMain(PSInput input) : SV_Target {
  float3 albedo = albedoTex.Sample(linearSampler, input.uv).rgb;
  float3 c = lambert(input.normal, lightDir, albedo);
  return float4(c * helper(1.0), 1.0);
}
