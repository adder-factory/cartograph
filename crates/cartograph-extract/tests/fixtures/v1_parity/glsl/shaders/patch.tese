#version 450

layout(triangles, equal_spacing, ccw) in;

vec4 interpolate(vec4 a, vec4 b, vec4 c) {
  return gl_TessCoord.x * a + gl_TessCoord.y * b + gl_TessCoord.z * c;
}

void main() {
  gl_Position = interpolate(gl_in[0].gl_Position, gl_in[1].gl_Position, gl_in[2].gl_Position);
}
