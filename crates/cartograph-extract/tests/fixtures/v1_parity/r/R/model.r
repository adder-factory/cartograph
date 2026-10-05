library(dplyr)
library("tidyr")
require(stats)
require("ggplot2")
library(R"[mypkg]")
source("R/utils.R")
source(r"(scripts/setup.r)")
source(paste0("R", "/dynamic.R"))

fit_model <- function(df) {
  cleaned <- dplyr::filter(df, value > 0)
  scaled <- scale_values(cleaned$value)
  summary_stats(scaled)
}

summary_stats <- function(values) {
  m <- mean(values)
  s <- stats::sd(values)
  structure(list(mean = m, sd = s), class = "summary_box")
}
