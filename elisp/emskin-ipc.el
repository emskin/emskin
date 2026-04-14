;;; emskin-ipc.el --- IPC connection and protocol for emskin  -*- lexical-binding: t; -*-

(require 'json)

;; ---------------------------------------------------------------------------
;; Codec: 4-byte u32 LE length prefix + JSON payload
;; ---------------------------------------------------------------------------

(defun emskin--encode-message (msg)
  "Encode MSG (alist/plist) as a framed JSON message (unibyte string)."
  (let* ((json (encode-coding-string (json-encode msg) 'utf-8 t))
         (len (length json))
         (prefix (unibyte-string
                  (logand len #xff)
                  (logand (ash len -8) #xff)
                  (logand (ash len -16) #xff)
                  (logand (ash len -24) #xff))))
    (concat prefix json)))

(defun emskin--decode-next ()
  "Extract one complete message from `emskin--read-buf'.
Returns parsed JSON (hash-table) or nil if more data is needed.
Coerces buffer to unibyte so aref always yields raw byte values 0-255."
  (when (>= (length emskin--read-buf) 4)
    (let* ((b0 (aref emskin--read-buf 0))
           (b1 (aref emskin--read-buf 1))
           (b2 (aref emskin--read-buf 2))
           (b3 (aref emskin--read-buf 3))
           (len (+ b0 (ash b1 8) (ash b2 16) (ash b3 24))))
      (when (>= (length emskin--read-buf) (+ 4 len))
        (let* ((payload (decode-coding-string
                         (substring emskin--read-buf 4 (+ 4 len)) 'utf-8))
               (obj (json-parse-string payload)))
          (setq emskin--read-buf
                (substring emskin--read-buf (+ 4 len)))
          obj)))))

;; ---------------------------------------------------------------------------
;; Socket discovery
;; ---------------------------------------------------------------------------

(defun emskin--ipc-path ()
  "Return the IPC socket path, auto-discovering via parent PID when needed."
  (or emskin-ipc-path
      (let* ((ppid (string-trim
                    (shell-command-to-string
                     (format "cat /proc/%d/status | awk '/^PPid:/{print $2}'"
                             (emacs-pid)))))
             (runtime-dir (or (getenv "XDG_RUNTIME_DIR") "/tmp")))
        (format "%s/emskin-%s.ipc" runtime-dir ppid))))

;; ---------------------------------------------------------------------------
;; Process filter and sentinel
;; ---------------------------------------------------------------------------

(defun emskin--filter (proc data)
  "Accumulate DATA from PROC and dispatch complete messages."
  (ignore proc)
  (setq emskin--read-buf
        (concat emskin--read-buf (string-as-unibyte data)))
  (let (msg)
    (while (setq msg (emskin--decode-next))
      (emskin--dispatch msg))))

(defun emskin--sentinel (proc event)
  "Handle IPC connection state changes."
  (ignore proc)
  (when (string-match-p "\\(closed\\|failed\\|broken\\|finished\\)" event)
    (message "emskin: IPC connection %s" (string-trim event))
    (setq emskin--process nil)))

;; ---------------------------------------------------------------------------
;; Send / Connect
;; ---------------------------------------------------------------------------

(defun emskin--send (msg)
  "Send MSG (alist) to emskin over IPC."
  (when emskin--process
    (process-send-string emskin--process (emskin--encode-message msg))))

(defun emskin-connect ()
  "Connect to the emskin IPC socket (auto-discovers path)."
  (interactive)
  (when emskin--process
    (delete-process emskin--process)
    (setq emskin--process nil))
  (setq emskin--read-buf "")
  (let ((path (emskin--ipc-path)))
    (condition-case err
        (progn
          (setq emskin--process
                (make-network-process
                 :name "emskin-ipc"
                 :family 'local
                 :service path
                 :coding 'binary
                 :filter #'emskin--filter
                 :sentinel #'emskin--sentinel
                 :nowait nil))
          (message "emskin: connecting to %s" path))
      (error
       (message "emskin: failed to connect to %s: %s" path err)))))

(provide 'emskin-ipc)
;;; emskin-ipc.el ends here
