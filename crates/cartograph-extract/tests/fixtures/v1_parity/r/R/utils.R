#' Maximum rows processed per batch
MAX_N <- 100
COLORS <- c("red", "green")
threshold = 0.5

#' Add two numbers
#' @param a first
#' @param b second
add <- function(a, b) {
  a + b
}

subtract = function(a, b) a - b

divide <<- function(a, b) a / b

print.summary_box <- function(x, ...) {
  cat(format_label(x$value))
}

format_label <- function(value) {
  tmp <- paste0("value=", value)
  tmp
}

scale_values <- function(xs) {
  result <- lapply(xs, function(x) x * 2)
  total <- add(length(result), 0)
  divide(total, MAX_N)
}
