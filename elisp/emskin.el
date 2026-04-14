;;; emskin.el --- Emacs IPC client for the emskin Wayland compositor  -*- lexical-binding: t; -*-

(require 'json)
(require 'cl-lib)

;; ---------------------------------------------------------------------------
;; Customization
;; ---------------------------------------------------------------------------

(defgroup emskin nil
  "Interface to the emskin nested Wayland compositor."
  :prefix "emskin-"
  :group 'applications)

(defcustom emskin-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID."
  :type '(choice (const nil) string)
  :group 'emskin)

(defcustom emskin-crosshair nil
  "Non-nil to enable the crosshair overlay (caliper tool).
Shows crosshair lines and coordinates at the cursor position."
  :type 'boolean
  :group 'emskin
  :initialize #'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when (bound-and-true-p emskin--process)
           (emskin--send `((type . "set_crosshair")
                               (enabled . ,(if val t :json-false)))))))

(defcustom emskin-skeleton nil
  "Non-nil to enable the skeleton overlay (frame layout inspector).
Draws wireframe rectangles around frame chrome, every window, its
header-line/mode-line, and the echo area with coordinates and sizes."
  :type 'boolean
  :group 'emskin
  :initialize #'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when (bound-and-true-p emskin--process)
           (emskin--push-skeleton val))))

;; ---------------------------------------------------------------------------
;; Internal state
;; ---------------------------------------------------------------------------

(defvar emskin--process nil
  "The network process connected to emskin's IPC socket.")

(defvar emskin--read-buf ""
  "Accumulates raw bytes received from emskin.")

(defvar emskin--header-offset nil
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once from compositor-reported surface height.")

(defvar-local emskin--window-id nil
  "emskin window_id for the embedded app embedded in this buffer.")

(defvar-local emskin--visible nil
  "Whether this EAF buffer is currently displayed in an Emacs window.")


;; Mirror tracking: window-id → (source-emacs-window . mirror-alist)
;; mirror-alist: ((emacs-window-id . view-id) ...)
(defvar emskin--mirror-table (make-hash-table :test 'eql)
  "Tracks source and mirror windows per embedded app.
Key: window-id.  Value: (SOURCE-WIN . ((VIEW-ID . EMACS-WIN) ...)).")

(defvar emskin--last-focused-wid 'unset
  "Last window-id sent via set_focus IPC.  Used as change-detection guard.")

(defvar emskin--next-view-id 0
  "Counter for generating unique mirror view IDs.")

;; --- Workspace tracking ---
(defvar emskin--frame-workspace-table (make-hash-table :test 'eq)
  "Maps Emacs frame objects to compositor workspace IDs.")

(defvar emskin--pending-frame-queue nil
  "Frames awaiting workspace_created IPC confirmation (FIFO order).
Use `nconc' to append and `pop' to dequeue from front.")

(defvar emskin--active-workspace-id nil
  "Currently active workspace ID in the compositor.")

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
;; Process filter (calloop equivalent on the Emacs side)
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
  (when (string-match-p "\\(closed\\|failed\\|broken\\|finished\\)" event)
    (message "emskin: IPC connection %s" (string-trim event))
    (setq emskin--process nil)))

;; ---------------------------------------------------------------------------
;; Message dispatch
;; ---------------------------------------------------------------------------

(defun emskin--dispatch (msg)
  "Dispatch a parsed MSG hash-table from emskin."
  (let ((type (gethash "type" msg "")))
    (cond
     ((string= type "connected")
      (message "emskin: connected (version %s)" (gethash "version" msg "?"))
      ;; Initialize workspace tracking: initial frame = workspace 1.
      (setq emskin--active-workspace-id 1)
      (puthash (selected-frame) 1 emskin--frame-workspace-table)
      (when emskin-crosshair
        (emskin--send `((type . "set_crosshair") (enabled . t)))))
     ((string= type "error")
      (message "emskin error: %s" (gethash "msg" msg "")))
     ((string= type "window_created")
      (emskin--on-window-created (gethash "window_id" msg)
                                  (gethash "title" msg "")))
     ((string= type "window_destroyed")
      (emskin--on-window-destroyed (gethash "window_id" msg)))
     ((string= type "title_changed")
      (emskin--on-title-changed (gethash "window_id" msg)
                                 (gethash "title" msg "")))
     ((string= type "focus_view")
      (emskin--on-focus-view (gethash "window_id" msg)
                                 (gethash "view_id" msg)))
     ((string= type "surface_size")
      (let* ((h (gethash "height" msg))
             (offset (max 0 (- h (frame-pixel-height)))))
        (setq emskin--header-offset offset)
        (message "emskin: surface=%sx%s bars=%dpx"
                 (gethash "width" msg) h offset)
        ;; Re-sync EAF windows now that we have the correct offset.
        (dolist (frame (frame-list))
          (emskin--sync-frame frame))))
     ((string= type "skeleton_clicked")
      (let ((kind (gethash "kind" msg ""))
            (label (gethash "label" msg ""))
            (x (gethash "x" msg 0))
            (y (gethash "y" msg 0))
            (w (gethash "w" msg 0))
            (h (gethash "h" msg 0)))
        (message "emskin skeleton: %s%s (%d,%d) %dx%d"
                 kind
                 (if (string-empty-p label) "" (format " [%s]" label))
                 x y w h)))
     ((string= type "workspace_created")
      (emskin--on-workspace-created (gethash "workspace_id" msg)))
     ((string= type "workspace_switched")
      (emskin--on-workspace-switched (gethash "workspace_id" msg)))
     ((string= type "workspace_destroyed")
      (emskin--on-workspace-destroyed (gethash "workspace_id" msg)))
     (t
      (message "emskin: unknown message type %s" type)))))

(defun emskin--on-focus-view (window-id view-id)
  "Select the Emacs window that corresponds to WINDOW-ID / VIEW-ID.
VIEW-ID 0 means the source window; otherwise look up the mirror alist."
  (let* ((state (gethash window-id emskin--mirror-table))
         (target (when state
                   (if (= view-id 0)
                       (car state)
                     (cdr (assq view-id (cdr state)))))))
    ;; Fallback: search current frame only to avoid cross-workspace switch.
    (unless (and target (window-live-p target)
                 (eq (window-frame target) (selected-frame)))
      (when-let ((buf (emskin--find-buffer window-id)))
        ;; nil = search current frame only (not t = all frames).
        (setq target (get-buffer-window buf nil))))
    (when (and target (window-live-p target))
      (select-window target))))

(defun emskin--on-window-created (window-id title)
  "Create/display a buffer for the new embedded app and send initial geometry."
  (let* ((buf-name (format "*emskin: %s*" (if (string-empty-p title) "app" title)))
         (buf (get-buffer-create buf-name)))
    (with-current-buffer buf
      (setq-local emskin--window-id window-id)
      (setq-local mode-name "emskin")
      (setq-local buffer-read-only t)
      (setq-local left-fringe-width 0)
      (setq-local right-fringe-width 0)
      (setq-local left-margin-width 0)
      (setq-local right-margin-width 0)
      (setq-local cursor-type nil)
      (add-hook 'kill-buffer-hook #'emskin--kill-buffer-hook nil t)
      (add-hook 'post-command-hook #'emskin--post-command-prefix-done nil t))
    (display-buffer buf '((display-buffer-pop-up-window
                           display-buffer-use-some-window)
                          (inhibit-same-window . t)
                          (reusable-frames . nil)))
    (when-let ((win (get-buffer-window buf t)))
      (set-window-scroll-bars win 0 nil 0 nil)
      (emskin--report-geometry window-id win))
    (message "emskin: embedded app ready (id=%s)" window-id)))

(defun emskin--find-buffer (window-id)
  "Return the buffer whose `emskin--window-id' equals WINDOW-ID, or nil."
  (seq-find (lambda (buf)
              (equal (buffer-local-value 'emskin--window-id buf) window-id))
            (buffer-list)))

(defun emskin--on-window-destroyed (window-id)
  "Close all Emacs windows/buffer for WINDOW-ID and restore focus."
  (when-let ((buf (emskin--find-buffer window-id)))
    ;; Clear window-id first to prevent kill-buffer-hook from sending
    ;; a redundant "close" message back to the compositor.
    (with-current-buffer buf
      (setq-local emskin--window-id nil))
    ;; Delete ALL windows showing this buffer (source + mirrors).
    ;; Walk in reverse so deletion doesn't invalidate the list.
    (dolist (win (reverse (get-buffer-window-list buf nil t)))
      (when (cdr (window-list (window-frame win) 'no-minibuf))
        (delete-window win)))
    (kill-buffer buf)
    ;; Clean up mirror-table entry.
    (remhash window-id emskin--mirror-table)
    ;; After window/buffer removal, check if the now-selected buffer is
    ;; an emskin app and send set_focus so the compositor matches.
    (let ((next-wid (buffer-local-value 'emskin--window-id
                                        (window-buffer (selected-window)))))
      (emskin--send `((type . "set_focus")
                      (window_id . ,(or next-wid :json-null)))))
    (message "emskin: window %s destroyed" window-id)))

(defun emskin--on-title-changed (window-id title)
  "Rename the EAF buffer when the app title changes."
  (when-let ((buf (emskin--find-buffer window-id)))
    (with-current-buffer buf
      (rename-buffer (format "*emskin: %s*" title) t))))

;; ---------------------------------------------------------------------------
;; Lifecycle: kill-buffer → close
;; ---------------------------------------------------------------------------

(defun emskin--kill-buffer-hook ()
  "Notify emskin to close the app when its Emacs buffer is killed."
  (when emskin--window-id
    (emskin--send `((type . "close")
                        (window_id . ,emskin--window-id)))))

;; ---------------------------------------------------------------------------
;; Prefix key sequence: compositor redirects focus to Emacs for C-x, C-c, M-x.
;; After the command completes, tell compositor to restore app focus.
;; ---------------------------------------------------------------------------

(defun emskin--post-command-prefix-done ()
  "After a command completes in an EAF buffer, signal the compositor.
The compositor only acts if it previously redirected focus for a prefix key."
  (when emskin--process
    (emskin--send '((type . "prefix_done")))))

;; ---------------------------------------------------------------------------
;; Public API
;; ---------------------------------------------------------------------------

(defun emskin-toggle-crosshair ()
  "Toggle the crosshair overlay (caliper tool)."
  (interactive)
  (customize-set-variable 'emskin-crosshair (not emskin-crosshair)))

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

(defun emskin--send (msg)
  "Send MSG (alist) to emskin over IPC."
  (when emskin--process
    (process-send-string emskin--process (emskin--encode-message msg))))

;; ---------------------------------------------------------------------------
;; Geometry reporting
;; ---------------------------------------------------------------------------

(defun emskin--frame-header-offset (&optional _frame)
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once when the compositor reports the surface size."
  (or emskin--header-offset 0))

(defun emskin--window-geometry (window)
  "Return (x y w h) in pixels for Emacs WINDOW.
Coordinates are relative to the top-left of the Wayland surface.
Covers the body area (excludes fringes, margins, header-line, mode-line)."
  (let* ((body (window-body-pixel-edges window))
         (off (emskin--frame-header-offset (window-frame window)))
         (x (nth 0 body))
         (raw-y (nth 1 body))
         (y (+ raw-y off))
         (w (- (nth 2 body) x))
         (h (- (nth 3 body) raw-y)))
    (list x y w h)))

(defun emskin-debug-geometry ()
  "Print geometry debug info to *Messages*."
  (interactive)
  (let* ((frame (selected-frame))
         (geom (frame-geometry frame))
         (win (selected-window))
         (root-edges (window-pixel-edges (frame-root-window frame)))
         (mb-h (or (cdr (alist-get 'menu-bar-size geom)) 0))
         (tb-h (or (cdr (alist-get 'tool-bar-size geom)) 0))
         (mb-ext (alist-get 'menu-bar-external geom))
         (tb-ext (alist-get 'tool-bar-external geom))
         (outer-h (cdr (alist-get 'outer-size geom)))
         (pixel-h (frame-pixel-height frame))
         (inner-h (frame-inner-height frame))
         (mb-lines (frame-parameter frame 'menu-bar-lines))
         (offset (emskin--frame-header-offset frame))
         (final (emskin--window-geometry win)))
    (message (concat "emskin-debug: "
                     "mb: h=%d ext=%s lines=%s | "
                     "tb: h=%d ext=%s | "
                     "outer-h=%s pixel-h=%d inner-h=%d | "
                     "root-edges: %s | "
                     "offset: %d | final: %s")
             mb-h mb-ext mb-lines
             tb-h tb-ext
             outer-h pixel-h inner-h
             root-edges offset final)))

(defvar-local emskin--last-geometry nil
  "Last geometry sent for this buffer's EAF window, to skip no-op updates.")

(defun emskin--report-geometry (window-id window)
  "Send set_geometry for WINDOW-ID, only when geometry actually changed."
  (let ((geo (emskin--window-geometry window)))
    (unless (equal geo (buffer-local-value 'emskin--last-geometry
                                           (window-buffer window)))
      (with-current-buffer (window-buffer window)
        (setq-local emskin--last-geometry geo))
      (emskin--send `((type . "set_geometry")
                      (window_id . ,window-id)
                      (x . ,(nth 0 geo))
                      (y . ,(nth 1 geo))
                      (w . ,(nth 2 geo))
                      (h . ,(nth 3 geo)))))))

(defun emskin--alloc-view-id ()
  "Allocate a unique mirror view ID."
  (cl-incf emskin--next-view-id))

(defun emskin--send-mirror-geometry (wid view-id win msg-type)
  "Send mirror geometry IPC for WID/VIEW-ID at Emacs WIN position."
  (let ((geo (emskin--window-geometry win)))
    (emskin--send `((type . ,msg-type)
                    (window_id . ,wid)
                    (view_id . ,view-id)
                    (x . ,(nth 0 geo))
                    (y . ,(nth 1 geo))
                    (w . ,(nth 2 geo))
                    (h . ,(nth 3 geo))))))

(defun emskin--sync-frame (frame)
  "Sync visibility, geometry, and mirrors for EAF buffers in FRAME.
Only processes FRAME — never iterates other frames.  Skips the sync
if FRAME does not belong to the active workspace, which eliminates
race conditions during workspace switches (stale hooks fire for a
frame whose workspace-id doesn't match active-workspace-id yet)."
  (let ((ws-id (gethash frame emskin--frame-workspace-table)))
    (when (eql ws-id emskin--active-workspace-id)
    ;; Pass 1: collect Emacs windows showing each EAF buffer in this frame.
    (let ((wid-wins (make-hash-table :test 'eql)))
      (dolist (win (window-list frame 'no-minibuf))
        (when-let ((wid (buffer-local-value 'emskin--window-id
                                            (window-buffer win))))
          (set-window-scroll-bars win 0 nil 0 nil)
          (set-window-fringes win 0 0)
          (set-window-margins win 0 0)
          (puthash wid (append (gethash wid wid-wins) (list win)) wid-wins)))
      ;; Pass 2: for each EAF buffer, sync source + mirrors.
      (dolist (buf (buffer-list))
        (when-let ((wid (buffer-local-value 'emskin--window-id buf)))
          (let* ((wins (gethash wid wid-wins))
                 (now-visible (and wins t))
                 (was-visible (buffer-local-value 'emskin--visible buf))
                 (prev-state (gethash wid emskin--mirror-table))
                 (prev-source (car prev-state))
                 (prev-mirrors (cdr prev-state)))
            ;; Visibility change.
            (unless (eq now-visible was-visible)
              (with-current-buffer buf
                (setq-local emskin--visible now-visible))
              (emskin--send `((type . "set_visibility")
                              (window_id . ,wid)
                              (visible . ,(if now-visible t :json-false)))))
            (if (not wins)
                ;; No windows showing this buffer — clean up mirrors.
                (progn
                  (dolist (m prev-mirrors)
                    (emskin--send `((type . "remove_mirror")
                                    (window_id . ,wid)
                                    (view_id . ,(car m)))))
                  (remhash wid emskin--mirror-table))
              ;; Determine source window: keep prev-source if still showing,
              ;; otherwise use first window in the list.
              (let* ((source-win (if (and prev-source (memq prev-source wins))
                                     prev-source
                                   (car wins)))
                     (mirror-wins (remq source-win wins))
                     (new-mirrors nil))
                ;; Source changed — remove all old mirrors and rebuild.
                (when (and prev-source (not (eq source-win prev-source)))
                  (dolist (m prev-mirrors)
                    (emskin--send `((type . "remove_mirror")
                                    (window_id . ,wid)
                                    (view_id . ,(car m)))))
                  (setq prev-mirrors nil))
                ;; Sync source geometry.
                (emskin--report-geometry wid source-win)
                ;; Reconcile mirrors.
                (let ((old-by-win (make-hash-table :test 'eq)))
                  (dolist (m prev-mirrors)
                    (puthash (cdr m) (car m) old-by-win))
                  (dolist (mw mirror-wins)
                    (let ((vid (or (gethash mw old-by-win)
                                   (emskin--alloc-view-id))))
                      (push (cons vid mw) new-mirrors)
                      (if (gethash mw old-by-win)
                          (emskin--send-mirror-geometry
                           wid vid mw "update_mirror_geometry")
                        (emskin--send-mirror-geometry
                         wid vid mw "add_mirror"))
                      (remhash mw old-by-win)))
                  (maphash (lambda (_win vid)
                             (emskin--send `((type . "remove_mirror")
                                             (window_id . ,wid)
                                             (view_id . ,vid))))
                           old-by-win))
                (puthash wid (cons source-win (nreverse new-mirrors))
                         emskin--mirror-table))))))))))

(add-hook 'window-size-change-functions #'emskin--sync-frame)
(add-hook 'window-buffer-change-functions #'emskin--sync-frame)

;; ---------------------------------------------------------------------------
;; Skeleton overlay (frame layout inspector)
;; ---------------------------------------------------------------------------

(defun emskin--skeleton-rect (kind label x y w h selected)
  "Build one skeleton rect alist."
  `((kind . ,kind)
    (label . ,(or label ""))
    (x . ,x)
    (y . ,y)
    (w . ,w)
    (h . ,h)
    (selected . ,(if selected t :json-false))))

(defun emskin--collect-skeleton-rects ()
  "Return a list of rect alists describing the selected frame's layout.
Coordinates are in pixels relative to the top-left of the Wayland surface,
matching the convention used by `emskin--window-geometry'."
  (let* ((frame (selected-frame))
         (geom (frame-geometry frame))
         (selected-win (selected-window))
         (off (emskin--frame-header-offset frame))
         ;; On pgtk, `outer-size' in `frame-geometry' does NOT include the
         ;; external GTK menu-bar / tool-bar heights (same architectural
         ;; limitation as `menu-bar-size'). Compute the true surface height
         ;; from `frame-pixel-height' + chrome offset so the frame rect
         ;; actually wraps the whole compositor window.
         (outer-w (frame-pixel-width frame))
         (outer-h (+ (frame-pixel-height frame) off))
         (mb-on (> (or (frame-parameter frame 'menu-bar-lines) 0) 0))
         (tb-on (> (or (frame-parameter frame 'tool-bar-lines) 0) 0))
         (tab-on (> (or (frame-parameter frame 'tab-bar-lines) 0) 0))
         (raw-mb-h (if mb-on (or (cdr (alist-get 'menu-bar-size geom)) 0) 0))
         (raw-tb-h (if tb-on (or (cdr (alist-get 'tool-bar-size geom)) 0) 0))
         (tab-h    (if tab-on (or (cdr (alist-get 'tab-bar-size geom)) 0) 0))
         ;; pgtk reports 0 for external GTK bar sizes. If either is 0 but
         ;; the total chrome offset is larger than the known side, derive
         ;; the missing one so both bars can be drawn in their correct
         ;; positions instead of stacking at y=0.
         (mb-h (cond ((not mb-on) 0)
                     ((and (zerop raw-mb-h) (> off raw-tb-h)) (- off raw-tb-h))
                     (t raw-mb-h)))
         (tb-h (cond ((not tb-on) 0)
                     ((and (zerop raw-tb-h) (> off raw-mb-h)) (- off raw-mb-h))
                     (t raw-tb-h)))
         (rects nil))
    ;; Frame outer rectangle.
    (push (emskin--skeleton-rect "frame" "" 0 0 outer-w outer-h nil) rects)
    ;; External chrome aggregate (menu-bar + tool-bar). Kept as a bounding
    ;; rect even when individual bars are drawn on top, so the label shows
    ;; the total `off` value for debugging.
    (when (> off 0)
      (push (emskin--skeleton-rect
             "chrome" (format "off=%d" off) 0 0 outer-w off nil)
            rects))
    ;; Menu bar (top of the external chrome).
    (when (> mb-h 0)
      (push (emskin--skeleton-rect "menu-bar" "" 0 0 outer-w mb-h nil)
            rects))
    ;; Tool bar (below the menu bar).
    (when (> tb-h 0)
      (push (emskin--skeleton-rect "tool-bar" "" 0 mb-h outer-w tb-h nil)
            rects))
    ;; Tab bar (internal, sits just below the external chrome).
    (when (> tab-h 0)
      (push (emskin--skeleton-rect "tab-bar" "" 0 off outer-w tab-h nil)
            rects))
    ;; Each live window: full rect + header-line strip + mode-line strip.
    (dolist (win (window-list frame 'no-minibuf))
      (let* ((edges (window-pixel-edges win))
             (body-edges (window-body-pixel-edges win))
             (raw-x (nth 0 edges))
             (raw-y (nth 1 edges))
             (raw-r (nth 2 edges))
             (raw-b (nth 3 edges))
             (body-top (nth 1 body-edges))
             (body-bot (nth 3 body-edges))
             (x raw-x)
             (y (+ raw-y off))
             (w (- raw-r raw-x))
             (h (- raw-b raw-y))
             (sel (eq win selected-win))
             (buf-title (buffer-name (window-buffer win))))
        (push (emskin--skeleton-rect "window" buf-title x y w h sel)
              rects)
        (when (> body-top raw-y)
          (push (emskin--skeleton-rect
                 "header-line" "" x y w (- body-top raw-y) nil)
                rects))
        (when (> raw-b body-bot)
          (push (emskin--skeleton-rect
                 "mode-line" "" x (+ body-bot off) w (- raw-b body-bot) nil)
                rects))))
    ;; Echo area / minibuffer window.
    (let ((mwin (minibuffer-window frame)))
      (when (and mwin (window-live-p mwin))
        (let* ((edges (window-pixel-edges mwin))
               (x (nth 0 edges))
               (y (+ (nth 1 edges) off))
               (w (- (nth 2 edges) (nth 0 edges)))
               (h (- (nth 3 edges) (nth 1 edges))))
          (when (and (> w 0) (> h 0))
            (push (emskin--skeleton-rect "echo-area" "" x y w h nil)
                  rects)))))
    (nreverse rects)))

(defvar emskin--last-skeleton-rects 'unset
  "Last rect list sent via set_skeleton IPC, used for change detection.
Auto-refresh hooks fire on every window-command, so without this guard
we'd re-send an identical payload on every keystroke.")

(defun emskin--push-skeleton (enabled)
  "Send the current skeleton state (bool ENABLED) to the compositor.
Skips IPC when the rect list is identical to the last one sent."
  (when emskin--process
    (if (not enabled)
        (unless (null emskin--last-skeleton-rects)
          (setq emskin--last-skeleton-rects nil)
          (emskin--send '((type . "set_skeleton")
                              (enabled . :json-false)
                              (rects . []))))
      (let ((rects (emskin--collect-skeleton-rects)))
        (unless (equal rects emskin--last-skeleton-rects)
          (setq emskin--last-skeleton-rects rects)
          (emskin--send
           `((type . "set_skeleton")
             (enabled . t)
             (rects . ,(vconcat rects)))))))))

(defun emskin-toggle-skeleton ()
  "Toggle the skeleton overlay (frame layout inspector)."
  (interactive)
  (customize-set-variable 'emskin-skeleton (not emskin-skeleton)))

(defun emskin-refresh-skeleton ()
  "Re-send the current frame layout as the skeleton overlay."
  (interactive)
  (when emskin-skeleton
    (emskin--push-skeleton t)))

(defun emskin--skeleton-auto-refresh (&optional _frame)
  "Hook: refresh skeleton when layout changes, only if the overlay is enabled."
  (when emskin-skeleton
    (emskin--push-skeleton t)))

(add-hook 'window-size-change-functions #'emskin--skeleton-auto-refresh)
(add-hook 'window-buffer-change-functions #'emskin--skeleton-auto-refresh)

(defun emskin--sync-focus (&optional _frame)
  "Tell the compositor which surface should have keyboard focus.
When the selected window shows an EAF buffer, focus the app;
otherwise focus Emacs.  Skips IPC when focus hasn't changed."
  (when emskin--process
    (let ((wid (buffer-local-value 'emskin--window-id
                                   (window-buffer (selected-window)))))
      (unless (eq wid emskin--last-focused-wid)
        (setq emskin--last-focused-wid wid)
        (emskin--send (if wid
                          `((type . "set_focus") (window_id . ,wid))
                        '((type . "set_focus"))))))))

(add-hook 'window-selection-change-functions #'emskin--sync-focus)

;; ---------------------------------------------------------------------------
;; Workspace management (C-x 5 frame operations)
;; ---------------------------------------------------------------------------

(defun emskin--on-workspace-created (workspace-id)
  "Associate the most recently created frame with WORKSPACE-ID."
  (if emskin--pending-frame-queue
      (let ((frame (pop emskin--pending-frame-queue)))
        (when (frame-live-p frame)
          (puthash frame workspace-id emskin--frame-workspace-table)
          (message "emskin: frame → workspace %d" workspace-id)
          ;; Sync immediately so scroll-bars are fixed and geometry is sent.
          (emskin--sync-frame frame)))
    ;; No pending frame — this is the initial workspace for the current frame.
    (puthash (selected-frame) workspace-id emskin--frame-workspace-table)))

(defun emskin--resync-workspace ()
  "Force full re-sync for the active workspace's frame.
Clears change detection then delegates to `emskin--sync-frame',
which handles source/mirror separation correctly."
  ;; Clear change detection so sync-frame fully re-processes.
  (dolist (buf (buffer-list))
    (when (buffer-local-value 'emskin--window-id buf)
      (with-current-buffer buf
        (setq-local emskin--last-geometry nil))))
  (when-let ((fr (emskin--active-frame)))
    (emskin--sync-frame fr)))

(defun emskin--active-frame ()
  "Return the Emacs frame for the active workspace, or nil."
  (let (result)
    (maphash (lambda (frame ws-id)
               (when (eql ws-id emskin--active-workspace-id)
                 (setq result frame)))
             emskin--frame-workspace-table)
    result))

(defun emskin--on-workspace-switched (workspace-id)
  "Update active workspace tracking and re-sync geometry."
  (setq emskin--active-workspace-id workspace-id)
  ;; Suppress after-focus-change to prevent feedback loop.
  (setq emskin--workspace-switch-suppressed t)
  (run-with-timer 0.3 nil (lambda () (setq emskin--workspace-switch-suppressed nil)))
  (setq emskin--last-focused-wid 'unset)
  ;; Force resync — bypass sync-all's change detection.
  (emskin--resync-workspace)
  (emskin--sync-focus (selected-window)))

(defun emskin--on-workspace-destroyed (workspace-id)
  "Clean up frame-workspace mapping for destroyed workspace."
  (maphash (lambda (frame ws-id)
             (when (eql ws-id workspace-id)
               (remhash frame emskin--frame-workspace-table)))
           emskin--frame-workspace-table))

(defun emskin--after-make-frame (frame)
  "Queue FRAME for workspace association when a non-child frame is created."
  (when (and emskin--process
             emskin--active-workspace-id
             ;; Child frames have parent-frame parameter — don't queue.
             (not (frame-parameter frame 'parent-frame)))
    (setq emskin--pending-frame-queue
          (nconc emskin--pending-frame-queue (list frame)))))

(defvar emskin--workspace-switch-suppressed nil
  "When non-nil, suppress workspace switch from after-focus-change.")

(defun emskin--after-focus-change ()
  "Detect frame switch and request compositor workspace switch."
  (when (and emskin--process
             (not emskin--workspace-switch-suppressed))
    (let* ((frame (selected-frame))
           (ws-id (gethash frame emskin--frame-workspace-table)))
      (when (and ws-id
                 (not (eql ws-id emskin--active-workspace-id)))
        ;; Suppress further switches until this one completes.
        (setq emskin--workspace-switch-suppressed t)
        (emskin--send `((type . "switch_workspace")
                        (workspace_id . ,ws-id)))
        ;; Clear suppression after compositor has had time to process.
        (run-with-timer 0.2 nil
                        (lambda ()
                          (setq emskin--workspace-switch-suppressed nil)))))))

(defun emskin--delete-frame-hook (frame)
  "Clean up workspace mapping when a frame is deleted."
  (remhash frame emskin--frame-workspace-table))

(defun emskin--advise-other-frame (orig-fn &optional arg &rest args)
  "Switch compositor workspace around `other-frame'.
Sends switch_workspace BEFORE so GTK can focus the target window.
Resync is handled by `emskin--on-workspace-switched' when the
compositor confirms the switch via IPC — NOT here, because
`active-workspace-id' is still stale at this point."
  (when emskin--process
    (let* ((n (or arg 1))
           (target (let ((f (selected-frame)))
                     (dotimes (_ (abs n))
                       (setq f (if (> n 0) (next-frame f) (previous-frame f))))
                     f))
           (ws-id (gethash target emskin--frame-workspace-table)))
      (when (and ws-id (not (eql ws-id emskin--active-workspace-id)))
        (emskin--send `((type . "switch_workspace")
                        (workspace_id . ,ws-id))))))
  (apply orig-fn arg args))

(advice-add 'other-frame :around #'emskin--advise-other-frame)

(add-hook 'after-make-frame-functions #'emskin--after-make-frame)
(add-function :after after-focus-change-function #'emskin--after-focus-change)
(add-hook 'delete-frame-functions #'emskin--delete-frame-hook)

;; ---------------------------------------------------------------------------
;; Launch an embedded application
;; ---------------------------------------------------------------------------

(defcustom emskin-demo-dir
  (expand-file-name
   "../demo"
   (file-name-directory
    (or load-file-name buffer-file-name
        "~/.emacs.d/site-lisp/emacs-application-framework/mvp/elisp/")))
  "Directory containing EAF demo/app Python scripts."
  :type 'directory
  :group 'emskin)

(defun emskin-open-app (app-name)
  "Launch embedded application APP-NAME (Python script in `emskin-demo-dir')."
  (interactive "sApp name: ")
  (let ((script (expand-file-name (format "%s.py" app-name) emskin-demo-dir)))
    (unless (file-exists-p script)
      (error "EAF script not found: %s" script))
    (start-process (format "emskin-%s" app-name) nil "python3" script)
    (message "emskin: launched %s" app-name)))

(defun emskin-open-native-app (command)
  "Launch a native Wayland application inside emskin.
COMMAND is a shell command string, e.g. \"foot\" or \"firefox\"."
  (interactive "sCommand: ")
  (let ((args (split-string-and-unquote command)))
    (apply #'start-process
           (format "emskin-%s" (car args))
           nil args)
    (message "emskin: launched native app: %s" command)))

;; ---------------------------------------------------------------------------
;; Auto-connect when running inside emskin
;; ---------------------------------------------------------------------------

(defun emskin-maybe-auto-connect ()
  "Connect to emskin IPC if we appear to be running inside emskin.
Checks for the emskin-specific socket file derived from our parent PID."
  (let ((path (emskin--ipc-path)))
    (when (file-exists-p path)
      (run-with-timer 0.5 nil #'emskin-connect))))

;; Hook into Emacs startup.
(add-hook 'emacs-startup-hook #'emskin-maybe-auto-connect)

(provide 'emskin)
;;; emskin.el ends here
