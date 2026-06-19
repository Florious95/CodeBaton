import { ReactNode, useEffect } from "react";
import { X } from "lucide-react";

export function Dialog({
  title,
  icon,
  width = 520,
  onClose,
  closeOnOverlay = true,
  children,
  footer,
}: {
  title: string;
  icon?: ReactNode;
  width?: number;
  onClose: () => void;
  closeOnOverlay?: boolean;
  children: ReactNode;
  footer?: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && closeOnOverlay) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, closeOnOverlay]);

  return (
    <div
      className="overlay"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && closeOnOverlay) onClose();
      }}
    >
      <div className="dialog" style={{ width }}>
        <div className="dialog-head">
          {icon}
          <span style={{ flex: 1 }}>{title}</span>
          {closeOnOverlay && (
            <button className="ghost tiny" onClick={onClose} aria-label="关闭">
              <X size={16} />
            </button>
          )}
        </div>
        <div className="dialog-body">{children}</div>
        {footer && <div className="dialog-foot">{footer}</div>}
      </div>
    </div>
  );
}
