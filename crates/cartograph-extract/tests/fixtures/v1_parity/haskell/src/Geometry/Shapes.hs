{-# LANGUAGE ScopedTypeVariables #-}
-- | Shapes and areas.
module Geometry.Shapes
  ( Shape (..)
  , Name (..)
  , HasArea (..)
  , Point (..)
  , square
  , describe
  ) where

import Data.List (sortBy)
import qualified Data.Map as Map
import Util.Strings (padLeft)

-- | A plane shape.
data Shape
  = Circle Double
  | Rect Double Double
  deriving (Show, Eq)

-- | A record with fields.
data Point = Point
  { px :: Double
  , py :: Double
  } deriving (Show)

newtype Name = Name String

type Radius = Double

class HasArea a where
  area :: a -> Double
  perimeter :: a -> Double

instance HasArea Shape where
  area (Circle r) = pi * r * r
  area (Rect w h) = w * h
  perimeter (Circle r) = 2 * pi * r
  perimeter (Rect w h) = 2 * (w + h)

square :: Int -> Int
square x = x * x

describe :: Shape -> String
describe s = padLeft 10 (show (area s))

sortShapes :: [Shape] -> [Shape]
sortShapes = sortBy (\a b -> compare (area a) (area b))

(<+>) :: Point -> Point -> Point
(<+>) (Point a b) (Point c d) = Point (a + c) (b + d)

lookupArea :: Map.Map String Shape -> String -> Maybe Double
lookupArea table key = fmap area (Map.lookup key table)
