(ns build
  (:require [babashka.fs :as fs]))

(defn clean [] (fs/delete-tree "target"))

(clean)
