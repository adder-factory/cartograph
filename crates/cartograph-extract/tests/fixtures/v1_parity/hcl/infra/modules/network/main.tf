variable "region" {
  type = string
}

variable "cidr" {
  type = string
}

resource "aws_vpc" "this" {
  cidr_block = var.cidr
  tags       = { Region = var.region }
}

resource "aws_subnet" "private" {
  vpc_id     = aws_vpc.this.id
  cidr_block = cidrsubnet(var.cidr, 8, 1)
}

output "vpc_id" {
  value = aws_vpc.this.id
}

output "subnet_id" {
  value = aws_subnet.private.id
}
