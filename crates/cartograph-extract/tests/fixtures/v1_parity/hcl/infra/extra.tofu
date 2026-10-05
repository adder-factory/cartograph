resource "null_resource" "tofu_only" {
  triggers = {
    bucket = aws_s3_bucket.assets.id
  }
}
