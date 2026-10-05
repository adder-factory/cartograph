module Util.Strings (padLeft, joinWith, Separator) where

import Data.Char (toUpper)

type Separator = String

padLeft :: Int -> String -> String
padLeft n s
  | length s >= n = s
  | otherwise = replicate (n - length s) ' ' ++ s

joinWith :: Separator -> [String] -> String
joinWith _ [] = ""
joinWith sep (x : xs) = x ++ concatMap (sep ++) xs

shout :: String -> String
shout = map toUpper

data Token = Word String | Space
  deriving (Show)

class Render a where
  render :: a -> String

instance Render Token where
  render (Word w) = w
  render Space = " "
