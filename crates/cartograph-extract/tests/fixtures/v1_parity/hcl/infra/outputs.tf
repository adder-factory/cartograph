output "bucket_id" {
  value = aws_s3_bucket.logs.id
}

output "bucket_arn" {
  value = aws_s3_bucket.logs.arn
}

output "vpc_id" {
  value = module.vpc.vpc_id
}

output "account_id" {
  value = data.aws_caller_identity.current.account_id
}

output "short_data" {
  value = data.foo
}

output "ids" {
  value = [for s in var.subnets : s.id]
}
