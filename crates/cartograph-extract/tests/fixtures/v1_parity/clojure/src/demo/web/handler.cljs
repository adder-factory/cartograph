(ns demo.web.handler
  (:require [demo.util :as u]))

(defn handle [req]
  (when (u/ready?)
    {:status 200 :body (:path req)}))

(defn render-page [title]
  [:div [:h1 title]])
