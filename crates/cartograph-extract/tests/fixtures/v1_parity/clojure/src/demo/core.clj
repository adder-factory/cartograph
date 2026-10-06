(ns demo.core
  (:require [clojure.string :as str]
            [demo.util :refer [helper]]
            [demo.web.handler :as handler]
            clojure.set)
  (:import (java.util UUID)))

(defonce default-name "world")

(def max-retries 3)

(def ^:private secret-key :k)

(defn greet
  "Greets a user."
  [name]
  (str/upper-case (helper name)))

(defn- hidden [x] (+ x 1))

(defn ^:private also-hidden [y] (* y 2))

(defn multi-arity
  ([] (multi-arity default-name))
  ([n] (greet n)))

(defmacro with-log [expr]
  (list 'do expr))

(defn run []
  (let [id (UUID/randomUUID)
        f (fn [v] (hidden v))]
    (println (greet "ana") id)
    (handler/handle {:path "/"})
    (f 2)
    (.toString id)))
