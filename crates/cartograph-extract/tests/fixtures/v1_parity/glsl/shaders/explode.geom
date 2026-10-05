#version 450

layout(triangles) in;
layout(triangle_strip, max_vertices = 3) out;

uniform float uAmount;

vec4 explode(vec4 position, vec3 normal) {
  return position + vec4(normal * uAmount, 0.0);
}

void main() {
  for (int i = 0; i < 3; ++i) {
    gl_Position = explode(gl_in[i].gl_Position, vec3(0.0, 0.0, 1.0));
    EmitVertex();
  }
  EndPrimitive();
}
