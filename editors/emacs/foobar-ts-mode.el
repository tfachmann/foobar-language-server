;;; foobar-ts-mode.el --- foobar with tree-sitter and eglot -*- lexical-binding: t; -*-

;; Emacs 29+ (built-in treesit and eglot).
;;
;; Grammar: Emacs 29 loads parsers up to ABI 14, the tree-sitter CLI generates ABI 15. In a clone of
;; https://github.com/tfachmann/tree-sitter-foobar, build one for Emacs without touching the
;; committed parser:
;;   tree-sitter generate --abi 14 -o /tmp/foobar-abi14/src
;;   tree-sitter build -o ~/.emacs.d/tree-sitter/libtree-sitter-foobar.so /tmp/foobar-abi14
;; (or put the library in a directory listed in `treesit-extra-load-path').
;; Language server: `foobar' must be on PATH; start it with M-x eglot (or `eglot-ensure' in a hook).
;;
;; Emacs does not read highlights.scm: the rules below restate it for Emacs' font-lock faces.

;;; Code:

(require 'treesit)

(defun foobar-ts-mode--font-lock-settings ()
  "Highlighting rules, mirroring tree-sitter-foobar/queries/highlights.scm."
  (treesit-font-lock-rules
   :language 'foobar :feature 'comment
   '((comment) @font-lock-comment-face)
   :language 'foobar :feature 'keyword
   '(["defines" "process" "template" "use" "include"] @font-lock-keyword-face)
   :language 'foobar :feature 'string
   '([(string) (raw_string)] @font-lock-string-face
     (include (path) @font-lock-string-face))
   :language 'foobar :feature 'definition
   '((section_process (name) @font-lock-function-name-face)
     (section_template (identifier) @font-lock-function-name-face)
     (use (identifier) @font-lock-function-call-face)
     (parameters (identifier) @font-lock-variable-name-face)
     (argument (identifier) @font-lock-variable-name-face)
     (entry (identifier) @font-lock-property-name-face))
   :language 'foobar :feature 'variable :override t
   '((variable (identifier) @font-lock-variable-use-face))))

;;;###autoload
(define-derived-mode foobar-ts-mode prog-mode "foobar"
  "Major mode for foobar files, using tree-sitter."
  (setq-local comment-start "# ")
  (setq-local comment-start-skip "#+\\s-*")
  (when (treesit-ready-p 'foobar)
    (treesit-parser-create 'foobar)
    (setq-local treesit-font-lock-settings (foobar-ts-mode--font-lock-settings))
    (setq-local treesit-font-lock-feature-list
                '((comment keyword string) (definition variable)))
    (treesit-major-mode-setup)))

;;;###autoload
(add-to-list 'auto-mode-alist '("\\.foobar\\'" . foobar-ts-mode))

(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(foobar-ts-mode . ("foobar" "lsp"))))

(provide 'foobar-ts-mode)
;;; foobar-ts-mode.el ends here
