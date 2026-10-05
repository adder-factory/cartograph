(defpackage #:demo.util
  (:use #:cl)
  (:export #:helper #:clamp))

(defpackage :demo.core
  (:use :cl :alexandria)
  (:import-from #:demo.util #:helper #:clamp)
  (:export #:greet #:main))
