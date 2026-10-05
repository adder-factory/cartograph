(in-package #:demo.core)

(require "asdf")
(require :uiop)

(use-package :demo.util)

(defparameter *default-name* "world")

(defun greet (name)
  (string-upcase (helper name)))

(defmacro with-log (expr)
  (list 'progn expr))

(defclass user ()
  ((name :initarg :name :accessor user-name)
   (level :initarg :level :initform 1)))

(define-condition my-error (error)
  ((reason :initarg :reason)))

(defgeneric describe-user (u))

(defmethod describe-user ((u user))
  (format t "~a" (user-name u)))

(defun area (r)
  (let ((p (make-point :x r :y r)))
    (* (point-x p) (point-y p))))

(defun main ()
  (let ((u (make-instance 'user :name *default-name*)))
    (with-log (greet (user-name u)))
    (describe-user u)
    (incf *counter*)
    (when (> (clamp 5 0 +max-level+) 3)
      (error 'my-error :reason "too big"))))
