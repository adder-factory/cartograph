#version 450

in vec3 vNormal;
in vec3 vWorldPos;
out vec4 fragColor;

uniform Light uLights[4];
uniform Material uMaterial;
uniform int uLightCount;

vec3 accumulate(vec3 pos, vec3 n) {
  vec3 total = vec3(0.0);
  for (int i = 0; i < uLightCount; ++i) {
    total += shade(uMaterial, uLights[i], pos, n);
  }
  return total;
}

void main() {
  vec3 color = accumulate(vWorldPos, vNormal);
  fragColor = vec4(pow(color, vec3(1.0 / 2.2)), 1.0);
}
