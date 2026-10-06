variable "region" {
  type    = string
  default = "us-east-1"
}

variable "prefix" {
  type = string
}

variable "bucket_prefix" {
  type = string
}

variable "environment" {
  type    = string
  default = "dev"
}

variable "subnets" {
  type = list(object({ id = string, cidr = string }))
}

variable "input" {
  type = map(string)
}

variable "maps" {
  type = list(any)
}

variable "versioning_status" {
  type    = string
  default = "Enabled"
}

variable {
}
