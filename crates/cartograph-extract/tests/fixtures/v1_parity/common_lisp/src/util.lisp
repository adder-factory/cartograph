(in-package #:demo.util)

(defvar *counter* 0)

(defconstant +max-level+ 10)

(defun helper (name)
  "Returns a padded name."
  (format nil "~a!" (string-trim " " name)))

(defun clamp (value low high)
  (max low (min value high)))

(defstruct point
  (x 0)
  (y 0))

(defstruct (rect (:conc-name r-))
  width
  height)
