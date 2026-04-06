;;; eaf-eafvil.el --- Emacs IPC client for the eafvil Wayland compositor  -*- lexical-binding: t; -*-

;; ---------------------------------------------------------------------------
;; Customization
;; ---------------------------------------------------------------------------

(defgroup eaf-eafvil nil
  "Interface to the eafvil nested Wayland compositor."
  :prefix "eaf-eafvil-"
  :group 'applications)

(defcustom eaf-eafvil-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID."
  :type '(choice (const nil) string)
  :group 'eaf-eafvil)

;; ---------------------------------------------------------------------------
;; Internal state
;; ---------------------------------------------------------------------------

(defvar eaf-eafvil--process nil
  "The network process connected to eafvil's IPC socket.")

(defvar eaf-eafvil--read-buf ""
  "Accumulates raw bytes received from eafvil.")

;; ---------------------------------------------------------------------------
;; Socket discovery
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--ipc-path ()
  "Return the IPC socket path, auto-discovering via parent PID when needed."
  (or eaf-eafvil-ipc-path
      (let* ((ppid (string-trim
                    (shell-command-to-string
                     (format "cat /proc/%d/status | awk '/^PPid:/{print $2}'"
                             (emacs-pid)))))
             (runtime-dir (or (getenv "XDG_RUNTIME_DIR") "/tmp")))
        (format "%s/eafvil-%s.ipc" runtime-dir ppid))))

;; ---------------------------------------------------------------------------
;; Codec: 4-byte u32 LE length prefix + JSON payload
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--encode-message (msg)
  "Encode MSG (alist/plist) as a framed JSON message (unibyte string)."
  (let* ((json (encode-coding-string (json-encode msg) 'utf-8 t))
         (len (length json))
         (prefix (unibyte-string
                  (logand len #xff)
                  (logand (ash len -8) #xff)
                  (logand (ash len -16) #xff)
                  (logand (ash len -24) #xff))))
    (concat prefix json)))

(defun eaf-eafvil--decode-next ()
  "Try to extract one complete message from `eaf-eafvil--read-buf'.
Returns the parsed JSON object (as a hash-table) or nil if not enough data."
  (when (>= (length eaf-eafvil--read-buf) 4)
    (let* ((b0 (aref eaf-eafvil--read-buf 0))
           (b1 (aref eaf-eafvil--read-buf 1))
           (b2 (aref eaf-eafvil--read-buf 2))
           (b3 (aref eaf-eafvil--read-buf 3))
           (len (+ b0 (ash b1 8) (ash b2 16) (ash b3 24))))
      (when (>= (length eaf-eafvil--read-buf) (+ 4 len))
        (let* ((payload (substring eaf-eafvil--read-buf 4 (+ 4 len)))
               (obj (json-parse-string payload)))
          (setq eaf-eafvil--read-buf (substring eaf-eafvil--read-buf (+ 4 len)))
          obj)))))

;; ---------------------------------------------------------------------------
;; Process filter (calloop equivalent on the Emacs side)
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--filter (proc data)
  "Accumulate DATA from PROC and dispatch complete messages."
  (ignore proc)
  (setq eaf-eafvil--read-buf (concat eaf-eafvil--read-buf data))
  (let (msg)
    (while (setq msg (eaf-eafvil--decode-next))
      (eaf-eafvil--dispatch msg))))

(defun eaf-eafvil--sentinel (proc event)
  "Handle IPC connection state changes."
  (when (string-match-p "\\(closed\\|failed\\|broken\\|finished\\)" event)
    (message "eafvil: IPC connection %s" (string-trim event))
    (setq eaf-eafvil--process nil)))

;; ---------------------------------------------------------------------------
;; Message dispatch
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--dispatch (msg)
  "Dispatch a parsed MSG hash-table from eafvil."
  (let ((type (gethash "type" msg "")))
    (cond
     ((string= type "connected")
      (message "eafvil: connected (version %s)" (gethash "version" msg "?")))
     ((string= type "error")
      (message "eafvil error: %s" (gethash "msg" msg "")))
     ((string= type "window_created")
      (eaf-eafvil--on-window-created (gethash "window_id" msg)
                                  (gethash "title" msg "")))
     ((string= type "window_destroyed")
      (eaf-eafvil--on-window-destroyed (gethash "window_id" msg)))
     ((string= type "title_changed")
      (eaf-eafvil--on-title-changed (gethash "window_id" msg)
                                 (gethash "title" msg "")))
     (t
      (message "eafvil: unknown message type %s" type)))))

;; Placeholders — will be replaced in M2.
(defun eaf-eafvil--on-window-created (window-id title)
  (message "eafvil: window_created id=%s title=%s" window-id title))

(defun eaf-eafvil--on-window-destroyed (window-id)
  (message "eafvil: window_destroyed id=%s" window-id))

(defun eaf-eafvil--on-title-changed (window-id title)
  (message "eafvil: title_changed id=%s title=%s" window-id title))

;; ---------------------------------------------------------------------------
;; Public API
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-connect ()
  "Connect to the eafvil IPC socket (auto-discovers path)."
  (interactive)
  (when eaf-eafvil--process
    (delete-process eaf-eafvil--process)
    (setq eaf-eafvil--process nil))
  (setq eaf-eafvil--read-buf "")
  (let ((path (eaf-eafvil--ipc-path)))
    (condition-case err
        (progn
          (setq eaf-eafvil--process
                (make-network-process
                 :name "eaf-eafvil-ipc"
                 :family 'local
                 :service path
                 :coding 'binary
                 :filter #'eaf-eafvil--filter
                 :sentinel #'eaf-eafvil--sentinel
                 :nowait nil))
          (message "eafvil: connecting to %s" path))
      (error
       (message "eafvil: failed to connect to %s: %s" path err)))))

(defun eaf-eafvil--send (msg)
  "Send MSG (alist) to eafvil over IPC."
  (when eaf-eafvil--process
    (process-send-string eaf-eafvil--process (eaf-eafvil--encode-message msg))))

;; ---------------------------------------------------------------------------
;; Geometry reporting
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--window-geometry (window)
  "Return (x y w h) in pixels for Emacs WINDOW."
  (let* ((edges (window-pixel-edges window))
         (x (nth 0 edges))
         (y (nth 1 edges))
         (w (- (nth 2 edges) x))
         (h (- (nth 3 edges) y)))
    (list x y w h)))

(defun eaf-eafvil--report-geometry (window-id window)
  "Send set_geometry for WINDOW-ID based on WINDOW's current pixel geometry."
  (let ((geo (eaf-eafvil--window-geometry window)))
    (eaf-eafvil--send `((type . "set_geometry")
                    (window_id . ,window-id)
                    (x . ,(nth 0 geo))
                    (y . ,(nth 1 geo))
                    (w . ,(nth 2 geo))
                    (h . ,(nth 3 geo))))))

;; ---------------------------------------------------------------------------
;; Auto-connect when running inside eafvil
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-maybe-auto-connect ()
  "Connect to eafvil IPC if we appear to be running inside eafvil.
Checks for the eaf-eafvil-specific socket file derived from our parent PID."
  (when (featurep 'pgtk)
    (let ((path (eaf-eafvil--ipc-path)))
      (when (file-exists-p path)
        (run-with-timer 0.5 nil #'eaf-eafvil-connect)))))

;; Hook into Emacs startup.
(add-hook 'emacs-startup-hook #'eaf-eafvil-maybe-auto-connect)

(provide 'eaf-eafvil)
;;; eaf-eafvil.el ends here
