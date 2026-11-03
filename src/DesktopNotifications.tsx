import type { Translations } from "./i18n";
import { useTranslation } from "./i18n";
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ArrowUpRight, KeyRound, Power, RefreshCw, Settings2, Trash2 } from "lucide-react";
import { MessageSourceIcon } from "./MessageSourceIcon";
import { nextQQPreview, type QQPreviewTiming } from "./qqNotificationTiming";
import type { MessageSource } from "./messagePreview";
import "./QQNotifications.css";

export type DesktopNotification = {
  id: string;
  title: string;
  body: string;
  createdAt: number;
};

type DesktopNotificationSnapshot = {
  revision: number;
  enabled: boolean;
  status: "disabled" | "waiting" | "listening" | "permissionDenied" | "permissionRequired" | "error";
  items: DesktopNotification[];
  error: string | null;
};

type DesktopNotificationEvent = {
  snapshot: DesktopNotificationSnapshot;
  newestId: string | null;
  windowVisible: boolean;
};

type DesktopNotificationCommands = {
  getState: string;
  setEnabled: string;
  clear: string;
  requestAccess: string;
  openSettings: string;
  openApp: string;
};

type DesktopNotificationConfig = {
  key: "qq" | "dingtalk";
  source: MessageSource;
  enabledKey: string;
  eventName: string;
  commands: DesktopNotificationCommands;
};

const QQ_CONFIG: DesktopNotificationConfig = {
  key: "qq",
  source: "qq",
  enabledKey: "focusd-island-qq-notifications-enabled",
  eventName: "qq-notifications-changed",
  commands: {
    getState: "get_qq_notification_state",
    setEnabled: "set_qq_notifications_enabled",
    clear: "clear_qq_notifications",
    requestAccess: "request_qq_notification_access",
    openSettings: "open_qq_notification_settings",
    openApp: "open_qq",
  },
};

const DINGTALK_CONFIG: DesktopNotificationConfig = {
  key: "dingtalk",
  source: "dingtalk",
  enabledKey: "focusd-island-dingtalk-notifications-enabled",
  eventName: "dingtalk-notifications-changed",
  commands: {
    getState: "get_dingtalk_notification_state",
    setEnabled: "set_dingtalk_notifications_enabled",
    clear: "clear_dingtalk_notifications",
    requestAccess: "request_dingtalk_notification_access",
    openSettings: "open_dingtalk_notification_settings",
    openApp: "open_dingtalk",
  },
};

// Windows notification-listener access is shared by every source. The QQ key predates
// DingTalk support and is still read so upgrading users do not see a second prompt.
const ACCESS_REQUESTED_KEY = "focusd-island-notification-access-requested";
const LEGACY_ACCESS_REQUESTED_KEY = "focusd-island-qq-notification-access-requested";

function hasRequestedAccess() {
  return window.localStorage.getItem(ACCESS_REQUESTED_KEY) === "true"
    || window.localStorage.getItem(LEGACY_ACCESS_REQUESTED_KEY) === "true";
}

function markAccessRequested() {
  window.localStorage.setItem(ACCESS_REQUESTED_KEY, "true");
  window.localStorage.setItem(LEGACY_ACCESS_REQUESTED_KEY, "true");
}

const EMPTY_SNAPSHOT: DesktopNotificationSnapshot = {
  revision: 0,
  enabled: false,
  status: "disabled",
  items: [],
  error: null,
};

function useDesktopNotifications(config: DesktopNotificationConfig, canPreview: boolean) {
  const t = useTranslation();
  const copy = t[config.key];
  const [snapshot, setSnapshot] = useState(EMPTY_SNAPSHOT);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState(isTauri);
  const [previewWindow, setPreviewWindow] = useState<{ id: string; expiresAt: number; receivedAt: number } | null>(null);
  const canPreviewRef = useRef(canPreview);
  const revisionRef = useRef(-1);
  const timingRef = useRef<QQPreviewTiming | null>(null);
  const actionPending = useRef(false);

  useLayoutEffect(() => {
    canPreviewRef.current = canPreview;
    if (!canPreview) {
      timingRef.current = null;
      setPreviewWindow(null);
    }
  }, [canPreview]);

  const acceptSnapshot = useCallback((next: DesktopNotificationSnapshot) => {
    if (next.revision < revisionRef.current) return false;
    revisionRef.current = next.revision;
    setSnapshot(next);
    return true;
  }, []);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<DesktopNotificationEvent>(config.eventName, ({ payload }) => {
        if (cancelled || !acceptSnapshot(payload.snapshot)) return;
        if (!payload.snapshot.enabled || !payload.windowVisible || !canPreviewRef.current) {
          setPreviewWindow(null);
          timingRef.current = null;
          return;
        }
        if (payload.newestId) {
          const receivedAt = Date.now();
          const { timing, expiresAt } = nextQQPreview(timingRef.current, receivedAt);
          timingRef.current = timing;
          setPreviewWindow(expiresAt === null ? null : { id: payload.newestId, expiresAt, receivedAt });
        }
      });
      if (cancelled) { unlisten(); return; }
      const next = await invoke<DesktopNotificationSnapshot>(config.commands.setEnabled, {
        enabled: window.localStorage.getItem(config.enabledKey) !== "false",
      });
      if (!cancelled) acceptSnapshot(next);
      if (!cancelled && next.enabled && !hasRequestedAccess()) {
        // Remember the attempt before invoking so a dismissed prompt never repeats on startup.
        markAccessRequested();
        await invoke<string>(config.commands.requestAccess);
      }
    })().catch((error: unknown) => {
      if (!cancelled) setActionError(String(error));
    }).finally(() => {
      if (!cancelled) setBusy(false);
    });
    return () => { cancelled = true; unlisten?.(); };
  }, [acceptSnapshot, config]);

  useEffect(() => {
    if (!previewWindow) return;
    const timer = window.setTimeout(() => setPreviewWindow(null), Math.max(0, previewWindow.expiresAt - Date.now()));
    return () => window.clearTimeout(timer);
  }, [previewWindow]);

  const run = useCallback(async (action: () => Promise<void>) => {
    if (actionPending.current) return;
    if (!isTauri()) {
      setActionError(copy.desktopOnly);
      return false;
    }
    actionPending.current = true;
    setBusy(true);
    setActionError(null);
    try {
      await action();
      return true;
    } catch (error) {
      setActionError(String(error));
      return false;
    } finally {
      actionPending.current = false;
      setBusy(false);
    }
  }, [copy]);

  const setEnabled = useCallback((enabled: boolean) => run(async () => {
    acceptSnapshot(await invoke<DesktopNotificationSnapshot>(config.commands.setEnabled, { enabled }));
    window.localStorage.setItem(config.enabledKey, String(enabled));
    if (!enabled) { timingRef.current = null; setPreviewWindow(null); }
    if (enabled && !hasRequestedAccess()) {
      markAccessRequested();
      await invoke<string>(config.commands.requestAccess);
    }
  }), [acceptSnapshot, config, run]);

  const requestAccess = useCallback(() => run(async () => {
    markAccessRequested();
    await invoke<string>(config.commands.requestAccess);
  }), [config, run]);

  const openSettings = useCallback(() => run(async () => {
    await invoke(config.commands.openSettings, {
      access: snapshot.status === "permissionDenied" || snapshot.status === "permissionRequired",
    });
  }), [config, run, snapshot.status]);

  const refresh = useCallback(() => run(async () => {
    acceptSnapshot(await invoke<DesktopNotificationSnapshot>(config.commands.getState));
  }), [acceptSnapshot, config, run]);

  const clear = useCallback(() => run(async () => {
    acceptSnapshot(await invoke<DesktopNotificationSnapshot>(config.commands.clear));
    setPreviewWindow(null);
  }), [acceptSnapshot, config, run]);

  const openSource = useCallback(() => run(async () => {
    await invoke(config.commands.openApp);
    setPreviewWindow(null);
  }), [config, run]);

  return {
    snapshot, busy, actionError, setEnabled, requestAccess, openSettings, refresh, clear, openSource,
    previewReceivedAt: canPreview ? previewWindow?.receivedAt ?? null : null,
    preview: canPreview && previewWindow
      ? snapshot.items.find((item) => item.id === previewWindow.id) ?? null
      : null,
  };
}

export function useQQNotifications(canPreview: boolean) {
  return useDesktopNotifications(QQ_CONFIG, canPreview);
}

export function useDingTalkNotifications(canPreview: boolean) {
  return useDesktopNotifications(DINGTALK_CONFIG, canPreview);
}

type DesktopNotificationsController = ReturnType<typeof useQQNotifications>;

export type QQNotificationsController = DesktopNotificationsController;
export type DingTalkNotificationsController = DesktopNotificationsController;

function NotificationControls({ controller, copy, compact = false }: { controller: DesktopNotificationsController; copy: Translations["qq"]; compact?: boolean }) {
  const { snapshot, busy, actionError, setEnabled, requestAccess, openSettings } = controller;
  const needsAccess = snapshot.status === "permissionRequired" || snapshot.status === "permissionDenied";
  return <div className={`qq-panel__controls${compact ? " qq-panel__controls--compact" : ""}`}>
    <div className="qq-panel__control-row">
      {compact ? <button className="icon-button message-receiver-toggle" type="button" role="switch" aria-checked={snapshot.enabled} aria-label={copy.receive} title={snapshot.enabled ? copy.pause : copy.resume} disabled={busy} onClick={() => void setEnabled(!snapshot.enabled)}><Power size={15} strokeWidth={1.8} /></button> : <label className="qq-panel__toggle" title={snapshot.enabled ? copy.pause : copy.resume}>
        <span>{copy.title}</span>
        <span className="message-status" data-state={snapshot.status} role="status" aria-label={copy.status[snapshot.status]} title={copy.status[snapshot.status]} />
        <input type="checkbox" role="switch" aria-label={copy.receive} checked={snapshot.enabled} disabled={busy} onChange={(event) => void setEnabled(event.currentTarget.checked)} />
      </label>}
      {snapshot.enabled && (needsAccess || snapshot.status === "error" || snapshot.error || actionError) && <div className="qq-panel__actions">
        {snapshot.status === "permissionRequired" && <button className="message-action" type="button" title={copy.allow} aria-label={copy.allowLabel} disabled={busy} onClick={() => void requestAccess()}><KeyRound size={14} strokeWidth={1.8} />{copy.grant}</button>}
        {(snapshot.status === "permissionDenied" || snapshot.error || actionError) && <button className="icon-button" type="button" title={copy.settings} aria-label={copy.settings} disabled={busy} onClick={() => void openSettings()}><Settings2 size={16} strokeWidth={1.8} /></button>}
        {snapshot.status === "error" && <button className="icon-button" type="button" title={copy.retry} aria-label={copy.retryLabel} disabled={busy} onClick={() => void requestAccess()}><KeyRound size={16} strokeWidth={1.8} /></button>}
      </div>}
    </div>
    {!compact && needsAccess && <p className="qq-panel__error" role="alert">{copy.status[snapshot.status]}</p>}
    {!compact && (actionError || snapshot.error) && <p className="qq-panel__error" role="alert">{actionError || snapshot.error}</p>}
  </div>;
}

export function QQNotificationControls({ controller, compact = false }: { controller: QQNotificationsController; compact?: boolean }) {
  return <NotificationControls controller={controller} copy={useTranslation().qq} compact={compact} />;
}

export function DingTalkNotificationControls({ controller, compact = false }: { controller: DingTalkNotificationsController; compact?: boolean }) {
  return <NotificationControls controller={controller} copy={useTranslation().dingtalk} compact={compact} />;
}

function NotificationPanel({
  controller,
  copy,
  source,
  navigation,
}: {
  controller: DesktopNotificationsController;
  copy: Translations["qq"];
  source: MessageSource;
  navigation: ReactNode;
}) {
  const { snapshot, busy, actionError, refresh, clear, openSource } = controller;
  const t = useTranslation();
  const error = actionError || snapshot.error || (snapshot.status === "permissionRequired" || snapshot.status === "permissionDenied" ? copy.status[snapshot.status] : null);
  return (
    <section className="qq-panel" aria-label={copy.recent}>
      <header className="qq-panel__header">
        {navigation}
        <div className="qq-panel__toolbar">
          <NotificationControls controller={controller} copy={copy} compact />
          <button className="icon-button" type="button" title={copy.open} aria-label={copy.open} disabled={busy} onClick={() => void openSource()}><ArrowUpRight size={16} strokeWidth={1.8} /></button>
          <button className="icon-button" type="button" title={t.common.refresh} aria-label={copy.refresh} disabled={busy} onClick={() => void refresh()}><RefreshCw size={16} strokeWidth={1.8} /></button>
          <button className="icon-button" type="button" title={t.common.clear} aria-label={copy.clear} disabled={busy || snapshot.items.length === 0} onClick={() => void clear()}><Trash2 size={16} strokeWidth={1.8} /></button>
        </div>
      </header>
      {error && <p className="qq-panel__error" role="alert">{error}</p>}
      <div className="qq-panel__list" role="list" aria-label={copy.list} aria-busy={busy}>
        {snapshot.items.length === 0 ? (
          <div className="qq-panel__empty" role="status"><span className="message-empty-icon"><MessageSourceIcon source={source} size={28} /></span><span>{t.common.noMessages}</span></div>
        ) : snapshot.items.map((item) => (
          <article className="qq-panel__item" key={item.id} role="listitem">
            <button type="button" disabled={busy} onClick={() => void openSource()} aria-label={copy.openNotification(item.title)}>
              <div className="qq-panel__item-heading"><strong>{item.title}</strong><time dateTime={new Date(item.createdAt).toISOString()}>{new Date(item.createdAt).toLocaleTimeString(t.localeTag, { hour: "2-digit", minute: "2-digit" })}</time></div>
              {item.body && <p>{item.body}</p>}
            </button>
          </article>
        ))}
      </div>
    </section>
  );
}

export function QQNotificationPanel({ controller, navigation }: { controller: QQNotificationsController; navigation: ReactNode }) {
  return <NotificationPanel controller={controller} copy={useTranslation().qq} source="qq" navigation={navigation} />;
}

export function DingTalkNotificationPanel({ controller, navigation }: { controller: DingTalkNotificationsController; navigation: ReactNode }) {
  return <NotificationPanel controller={controller} copy={useTranslation().dingtalk} source="dingtalk" navigation={navigation} />;
}
