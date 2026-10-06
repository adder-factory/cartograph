#define PI 3.14159265
#define MAX_LIGHTS 4

struct Light {
  vec3 position;
  vec3 color;
  float intensity;
};

struct Material {
  vec3 albedo;
  float roughness;
};

const float EPSILON = 0.0001;

float square(float x) {
  return x * x;
}

vec3 computeNormal(vec3 n) {
  return normalize(n);
}

float attenuation(Light light, vec3 pos) {
  float d = length(light.position - pos);
  return light.intensity / max(square(d), EPSILON);
}

vec3 shade(Material m, Light light, vec3 pos, vec3 n) {
  vec3 l = normalize(light.position - pos);
  float ndotl = max(dot(computeNormal(n), l), 0.0);
  return m.albedo * light.color * ndotl * attenuation(light, pos);
}
