;;; emskin-ipc.el --- IPC connection and protocol for emskin  -*- lexical-binding: t; -*-

(require 'json)

;; ---------------------------------------------------------------------------
;; Hooks
;; ---------------------------------------------------------------------------

(defvar emskin-connected-hook nil
  "Hook run after the IPC connection to emskin is (re-)established.
Each effect module adds its `--sync' helper here so it can push its
current variable value to the compositor without the main file
having to know about every effect.")

(defsubst emskin--jbool (val)
  "Coerce VAL to a JSON-compatible boolean (`t' or `:json-false')."
  (if val t :json-false))

(defmacro emskin-define-toggle (name &optional label)
  "Generate the toggle command + connected-hook registration for effect NAME.

This is the low-level primitive used by every effect module to avoid
hand-rolling the identical toggle/hook boilerplate.  Use it when the
sync payload isn't a plain `(enabled . BOOL)' — for the plain case,
see the higher-level `emskin-define-bool-effect'.

NAME is a symbol naming the effect (e.g. `skeleton', `jelly-cursor').
Hyphens are preserved in every derived identifier.  LABEL is the
human-readable word shown in the status message; it defaults to
NAME's printed form (so `jelly-cursor' → \"jelly-cursor\"; pass
\"jelly cursor\" to make it more natural).

Expansion references these symbols, which must exist BEFORE the
expansion runs (i.e. at load time of the module that calls this
macro):
  - `emskin-NAME'          user variable           (declared in `emskin.el')
  - `emskin--NAME-sync'    sync helper, 0-arity   (defined in this module)
  - `emskin--process'      IPC process or nil     (from `emskin.el')
  - `emskin-connected-hook' normal hook           (from `emskin-ipc.el')

Expansion produces two top-level forms:
  1. `(defun emskin-toggle-NAME () (interactive) ...)' — flips the
     variable, calls the sync when connected, prints status message.
  2. `(add-hook 'emskin-connected-hook #'emskin--NAME-sync)' so the
     sync also fires on every IPC (re-)connect.

Example (full relevant code from `emskin-skeleton.el'):

  (defun emskin--skeleton-sync ()
    (emskin--push-skeleton emskin-skeleton))
  (emskin-define-toggle skeleton)

The second line expands (use `M-x pp-macroexpand-last-sexp' to see
this yourself) to:

  (progn
    (defun emskin-toggle-skeleton ()
      \"Toggle `emskin-skeleton' and push the change to the compositor.\"
      (interactive)
      (setq emskin-skeleton (not emskin-skeleton))
      (when emskin--process (emskin--skeleton-sync))
      (message \"emskin: %s %s\" \"skeleton\" (if emskin-skeleton \"ON\" \"OFF\")))
    (add-hook 'emskin-connected-hook #'emskin--skeleton-sync))"
  (let* ((name-str (symbol-name name))
         (var (intern (format "emskin-%s" name-str)))
         (sync (intern (format "emskin--%s-sync" name-str)))
         (toggle (intern (format "emskin-toggle-%s" name-str)))
         (label-str (or label name-str)))
    `(progn
       (defun ,toggle ()
         ,(format "Toggle `%s' and push the change to the compositor." var)
         (interactive)
         (setq ,var (not ,var))
         (when emskin--process (,sync))
         (message "emskin: %s %s" ,label-str (if ,var "ON" "OFF")))
       (add-hook 'emskin-connected-hook #',sync))))

(defmacro emskin-define-bool-effect (name ipc-type &optional label)
  "Define a simple boolean effect end-to-end in one line.

Use this when the sync is just `(enabled . BOOL)' — no composite
payload, no extra elisp-side state to reset on disable.  For anything
more, hand-write `emskin--NAME-sync' and call the lower-level
`emskin-define-toggle' directly (see `emskin-skeleton.el' and
`emskin-jelly.el').

Arguments:
  NAME      — symbol naming the effect (e.g. `measure')
  IPC-TYPE  — compositor message type string (e.g. \"set_measure\")
  LABEL     — optional display label, forwarded to
              `emskin-define-toggle'

Prerequisites (same as `emskin-define-toggle' plus):
  - `emskin-NAME' is declared in `emskin.el' with a real docstring.
    The forward `defvar' emitted here only suppresses byte-compiler
    free-variable warnings; the authoritative declaration must be in
    the main file.

Expansion adds three forms:
  1. forward `(defvar emskin-NAME)' — no default, no docstring
  2. `(defun emskin--NAME-sync () ...)' — sends an IPC frame with
     type=IPC-TYPE and enabled=NAME-as-JSON-bool
  3. `(emskin-define-toggle NAME LABEL)' — toggle + hook registration

Example (full content of `emskin-measure.el', minus file boilerplate):

  (require 'emskin-ipc)
  (emskin-define-bool-effect measure \"set_measure\")
  (provide 'emskin-measure)

The middle line expands to:

  (progn
    (defvar emskin-measure)
    (defun emskin--measure-sync ()
      (emskin--send `((type . \"set_measure\")
                      (enabled . ,(emskin--jbool emskin-measure)))))
    (defun emskin-toggle-measure ()
      \"Toggle `emskin-measure' and push the change to the compositor.\"
      (interactive)
      (setq emskin-measure (not emskin-measure))
      (when emskin--process (emskin--measure-sync))
      (message \"emskin: %s %s\" \"measure\" (if emskin-measure \"ON\" \"OFF\")))
    (add-hook 'emskin-connected-hook #'emskin--measure-sync))

The real `(defvar emskin-measure nil \"...\")' with docstring lives
in `emskin.el'; the forward `defvar' above just silences the
byte-compiler."
  (let ((sync (intern (format "emskin--%s-sync" (symbol-name name))))
        (var (intern (format "emskin-%s" (symbol-name name)))))
    `(progn
       (defvar ,var)
       (defun ,sync ()
         (emskin--send `((type . ,,ipc-type)
                         (enabled . ,(emskin--jbool ,var)))))
       (emskin-define-toggle ,name ,label))))

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
