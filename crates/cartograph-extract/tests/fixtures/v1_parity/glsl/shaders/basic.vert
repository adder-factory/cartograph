#version 450

layout(location = 0) in vec3 inPosition;
layout(location = 1) in vec3 inNormal;

layout(location = 0) out vec3 vNormal;
layout(location = 1) out vec3 vWorldPos;

uniform mat4 uModel;
uniform mat4 uViewProj;

vec4 toClip(vec3 p) {
  return uViewProj * uModel * vec4(p, 1.0);
}

void main() {
  vWorldPos = (uModel * vec4(inPosition, 1.0)).xyz;
  vNormal = mat3(uModel) * inNormal;
  gl_Position = toClip(inPosition);
}
