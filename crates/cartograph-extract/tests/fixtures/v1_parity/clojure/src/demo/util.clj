(ns demo.util)

(defn helper
  "Returns the trimmed name."
  [s]
  (clojure.string/trim s))

(defn ready? [] (helper " x "))

(defn swap-state! [state f]
  (swap! state f))

(defprotocol Greeter
  (greet-with [this msg]))

(defrecord Person [name age]
  Greeter
  (greet-with [this msg] (str msg (:name this))))

(defmulti area :shape)

(defmethod area :circle [{:keys [r]}] (* 3 r r))
