float luminance(float3 c) {
  return dot(c, float3(0.2126, 0.7152, 0.0722));
}
