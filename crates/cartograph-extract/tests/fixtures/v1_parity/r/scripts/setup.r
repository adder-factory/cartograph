source("R/model.r")
DATA_PATH <- "data/input.csv"

run <- function() {
  df <- read.csv(DATA_PATH)
  result <- fit_model(df)
  print(result)
  add(1, 2)
}

run()
