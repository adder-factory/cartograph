terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = { source = "hashicorp/aws" }
  }
}

provider "aws" {
  region = var.region
}

locals {
  name      = "${var.prefix}-app"
  full_name = "${var.bucket_prefix}-${var.region}"
  tags      = { Env = var.environment, Name = local.name }
  upper_ids = [for s in var.subnets : upper(s.id)]
  labelled  = { for k, v in var.input : k => "${v}-suffix" }
  enabled   = [for item in var.maps : item.id if item.enabled]
  mixed     = [for s in var.subnets : "${s.cidr}-${var.region}"]
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["self"]
}

data "aws_caller_identity" "current" {}

resource "aws_instance" "web" {
  count     = 3
  ami       = data.aws_ami.ubuntu.id
  subnet_id = module.vpc.subnet_id
  tags = {
    Name  = local.name
    Index = count.index
    Self  = self.id
    Path  = path.module
    Ws    = terraform.workspace
    Each  = each.key
  }
}

resource "aws_s3_bucket" "logs" {
  bucket = local.full_name
  tags   = local.tags
}

resource "aws_s3_bucket" "assets" {
  bucket = "${local.name}-assets"
}

resource "aws_s3_bucket_versioning" "v" {
  bucket = aws_s3_bucket.logs.id
  versioning_configuration {
    status = var.versioning_status
    rule {
      mfa_delete = local.tags
    }
  }
}

resource "aws_s3_bucket" {
  bucket = "missing-name-label"
}

module "vpc" {
  source = "./modules/network"
  region = var.region
  cidr   = local.full_name
}

module "registry" {
  source = "terraform-aws-modules/vpc/aws"
}

module "dynamic_src" {
  source = "./modules/${var.region}"
}

module "expr_src" {
  source = local.name
}

module "no_src" {
  count = 2
}

check "health" {
  assert {
    condition     = aws_instance.web[0].id != null
    error_message = "instance missing"
  }
}

moved {
  from = aws_s3_bucket.old
  to   = aws_s3_bucket.logs
}
