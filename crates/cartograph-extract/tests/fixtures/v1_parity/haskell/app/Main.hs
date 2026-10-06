module Main (main) where

import Geometry.Shapes
import qualified Util.Strings as S

main :: IO ()
main = do
  let shapes = [Circle 1.0, Rect 2.0 3.0]
  mapM_ (putStrLn . describe) shapes
  putStrLn (S.joinWith ", " (map describe shapes))
  print (square 4)
  where
    helper :: Int -> Int
    helper n = n + 1

total :: [Shape] -> Double
total xs = sum (map area xs)
