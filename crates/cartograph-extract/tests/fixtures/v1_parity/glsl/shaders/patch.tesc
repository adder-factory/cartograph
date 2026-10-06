#version 450

layout(vertices = 3) out;

uniform float uLevel;

float tessLevel(float base) {
  return max(base, 1.0);
}

void main() {
  gl_TessLevelInner[0] = tessLevel(uLevel);
  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;
}
