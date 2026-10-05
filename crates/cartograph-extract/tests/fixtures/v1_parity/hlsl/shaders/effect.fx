float4 tint;

float4 applyTint(float4 c) {
  return c * tint;
}

float4 main(float4 pos : POSITION) : SV_Position {
  return applyTint(pos);
}
