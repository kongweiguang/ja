// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as ToggleGroup from "@radix-ui/react-toggle-group";
import { Send, Square } from "lucide-react";
import { useRef, useState, type FormEvent, type KeyboardEvent, type ReactElement } from "react";
import type { Turn } from "@/ipc/runtimeEvents";
import { Button } from "@/components/primitives/Button";
import { cn } from "@/components/primitives/cn";
import "./composer.css";

export type ComposerAccessMode = Turn["accessMode"];

export interface ComposerModelOption {
  id: string;
  label: string;
}

export interface ComposerSubmit {
  text: string;
  accessMode: ComposerAccessMode;
  model?: string;
}

export interface ComposerProps {
  accessMode: ComposerAccessMode;
  model?: string;
  models?: readonly ComposerModelOption[];
  activeTurn?: boolean;
  disabled?: boolean;
  onAccessModeChange?: (accessMode: ComposerAccessMode) => void;
  onModelChange?: (model: string) => void;
  onSend: (request: ComposerSubmit) => void | Promise<void>;
  onCancel?: () => void | Promise<void>;
  className?: string;
}

const accessModes: readonly { value: ComposerAccessMode; label: string; description: string }[] = [
  { value: "read_only", label: "只读", description: "只查看，不写入文件" },
  { value: "workspace", label: "工作区", description: "可修改当前工作区" },
  { value: "full_access", label: "完全访问", description: "允许工作区外操作，命令仍需确认" },
];

/** Keeps display copy stable while protocol values stay English and typed. */
function accessModeLabel(mode: ComposerAccessMode): string {
  return accessModes.find((option) => option.value === mode)?.label ?? "工作区";
}

/**
 * Provides one small coding composer with no attachment or follow-up state;
 * the parent owns IPC and the component only emits typed intent callbacks.
 */
export function Composer({
  accessMode,
  model,
  models = [],
  activeTurn = false,
  disabled = false,
  onAccessModeChange,
  onModelChange,
  onSend,
  onCancel,
  className,
}: ComposerProps): ReactElement {
  const [text, setText] = useState("");
  const [localSending, setLocalSending] = useState(false);
  const [localCancelling, setLocalCancelling] = useState(false);
  const [error, setError] = useState<string>();
  const sendRef = useRef(false);
  const cancelRef = useRef(false);
  const hasActiveTurn = activeTurn || localSending;
  const canSend = text.trim().length > 0 && !disabled && !hasActiveTurn && !localCancelling;
  const canCancel = activeTurn && !disabled && !localCancelling && onCancel !== undefined;

  /** Serializes send intent so a rapid keyboard/click pair creates one turn. */
  const submit = async (): Promise<void> => {
    if (!canSend || sendRef.current) {
      return;
    }
    sendRef.current = true;
    setLocalSending(true);
    setError(undefined);
    try {
      await onSend({ text: text.trim(), accessMode, ...(model?.trim() ? { model: model.trim() } : {}) });
      setText("");
    } catch {
      setError("发送失败，请检查运行时连接后重试。");
    } finally {
      sendRef.current = false;
      setLocalSending(false);
    }
  };

  /** Cancellation stays available for the active thread and is independently guarded. */
  const cancel = async (): Promise<void> => {
    if (!canCancel || cancelRef.current || onCancel === undefined) {
      return;
    }
    cancelRef.current = true;
    setLocalCancelling(true);
    setError(undefined);
    try {
      await onCancel();
    } catch {
      setError("取消失败，请稍后重试。");
    } finally {
      cancelRef.current = false;
      setLocalCancelling(false);
    }
  };

  const handleSubmit = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault();
    void submit();
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  };

  return (
    <form className={cn("ja-composer", className)} onSubmit={handleSubmit} aria-label="发送消息">
      <textarea
        className="ja-composer__input"
        aria-label="消息"
        placeholder="描述你想完成的工作…"
        value={text}
        maxLength={1_048_576}
        rows={3}
        disabled={disabled || hasActiveTurn || localCancelling}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={handleKeyDown}
      />
      <div className="ja-composer__controls">
        <div className="ja-composer__settings">
          <label className="ja-composer__select-label">
            <span>模型</span>
            <select aria-label="模型" value={model ?? ""} disabled={disabled || hasActiveTurn} onChange={(event) => onModelChange?.(event.target.value)}>
              {models.length === 0 ? <option value="">默认模型</option> : null}
              {models.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}
            </select>
          </label>
          <fieldset className="ja-composer__mode-fieldset" disabled={disabled || hasActiveTurn}>
            <legend>访问模式</legend>
            <ToggleGroup.Root
              type="single"
              value={accessMode}
              aria-label="访问模式"
              onValueChange={(value) => {
                if (value !== "") {
                  onAccessModeChange?.(value as ComposerAccessMode);
                }
              }}
            >
              {accessModes.map((option) => (
                <ToggleGroup.Item key={option.value} value={option.value} aria-label={option.label} title={option.description} className="ja-composer__mode">
                  {option.label}
                </ToggleGroup.Item>
              ))}
            </ToggleGroup.Root>
          </fieldset>
          <span className="ja-composer__current-mode">{accessModeLabel(accessMode)}</span>
        </div>
        <div className="ja-composer__actions">
          {activeTurn ? <Button type="button" variant="ghost" size="sm" disabled={!canCancel} loading={localCancelling} onClick={() => void cancel()}><Square data-icon="inline-start" />取消</Button> : null}
          {!activeTurn ? <Button type="submit" variant="primary" size="sm" disabled={!canSend} loading={localSending}><Send data-icon="inline-start" />发送</Button> : null}
        </div>
      </div>
      <p className="ja-composer__hint">Enter 发送 · Shift+Enter 换行 · Shell 命令会先询问</p>
      {error ? <p className="ja-composer__error" role="alert">{error}</p> : null}
    </form>
  );
}
