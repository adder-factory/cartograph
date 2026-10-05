(ns demo.shared)

(def config {:port 8080})

(defn port [] (:port config))
