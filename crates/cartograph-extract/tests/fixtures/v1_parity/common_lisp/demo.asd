(defsystem "demo"
  :version "0.1.0"
  :depends-on ("alexandria")
  :components ((:module "src"
                :components ((:file "package")
                             (:file "util")
                             (:file "core")))))
