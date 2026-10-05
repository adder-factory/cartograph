# generic HCL configuration
target "app" {
  context    = "."
  dockerfile = "Dockerfile"
  tags       = ["example/app:latest"]
}

group "default" {
  targets = ["app"]
}
