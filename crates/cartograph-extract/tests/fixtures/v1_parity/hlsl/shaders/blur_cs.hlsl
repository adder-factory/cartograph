#include "post.fxh"

RWTexture2D<float4> output : register(u0);
Texture2D<float4> input : register(t1);

groupshared float4 cache[64];

float4 load(int2 p) {
  return input[p];
}

[numthreads(8, 8, 1)]
void CSMain(uint3 id : SV_DispatchThreadID, uint gi : SV_GroupIndex) {
  cache[gi] = load(int2(id.xy));
  GroupMemoryBarrierWithGroupSync();
  float l = luminance(cache[gi].rgb);
  output[id.xy] = float4(l, l, l, 1.0);
}
