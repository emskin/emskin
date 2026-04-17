;;; emskin-skeleton.el --- Skeleton overlay for emskin  -*- lexical-binding: t; -*-

(require 'emskin-app)

;; ---------------------------------------------------------------------------
;; Skeleton rect building
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
    ;; External chrome aggregate (menu-bar + tool-bar).
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

;; ---------------------------------------------------------------------------
;; Push / toggle
;; ---------------------------------------------------------------------------

(defvar emskin--last-skeleton-rects 'unset
  "Last rect list sent via set_skeleton IPC, used for change detection.")

(defvar emskin-skeleton)  ; defined in `emskin.el'

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

(defun emskin--skeleton-sync ()
  (emskin--push-skeleton emskin-skeleton))

(emskin-define-toggle skeleton)

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

(provide 'emskin-skeleton)
;;; emskin-skeleton.el ends here
