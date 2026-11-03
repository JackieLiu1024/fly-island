import { useTranslation } from "./i18n";
import { QQMailPanel, type QQMailController } from "./QQMail";
import {
  DingTalkNotificationPanel,
  QQNotificationPanel,
  type DingTalkNotificationsController,
  type QQNotificationsController,
} from "./DesktopNotifications";
import { WeChatReminderPanel, type WeChatRemindersController } from "./WeChatReminders";
import { MessageSourceIcon } from "./MessageSourceIcon";
import { mailBadgeCount, type MessageSource } from "./messagePreview";

export type MessageView = MessageSource;

type MessagesProps = {
  qqMail: QQMailController;
  qqNotifications: QQNotificationsController;
  dingtalkNotifications: DingTalkNotificationsController;
  wechatReminders: WeChatRemindersController;
  source: MessageView;
  onSourceChange: (source: MessageView) => void;
};

export function messagePanelHeight(source: MessageView, readingMail = false) {
  return source === "qqMail" && readingMail ? 450 : 350;
}

export function MessagesPanel({ qqMail, qqNotifications, dingtalkNotifications, wechatReminders, source, onSourceChange }: MessagesProps) {
  const t = useTranslation();
  const { snapshot: qq } = qqNotifications;
  const { snapshot: dingtalk } = dingtalkNotifications;
  const { snapshot: mail } = qqMail;
  const navigate = (next: MessageView) => onSourceChange(next);
  const navigation = (
    <div className="message-sources" role="group" aria-label={t.messages.sources}>
      <button type="button" aria-label={t.messages.qqNotifications} aria-pressed={source === "qq"} onClick={() => navigate("qq")}><MessageSourceIcon source="qq" /><span>QQ</span>{qq.items.length > 0 && <span className="message-source-count">{mailBadgeCount(qq.items.length)}</span>}</button>
      <button type="button" aria-label={t.messages.dingtalkNotifications} aria-pressed={source === "dingtalk"} onClick={() => navigate("dingtalk")}><MessageSourceIcon source="dingtalk" /><span>{t.messages.dingtalk}</span>{dingtalk.items.length > 0 && <span className="message-source-count">{mailBadgeCount(dingtalk.items.length)}</span>}</button>
      <button type="button" aria-label={t.messages.wechatReminders} aria-pressed={source === "wechat"} onClick={() => navigate("wechat")}><MessageSourceIcon source="wechat" /><span>{t.messages.wechat}</span>{wechatReminders.snapshot.pending && <span className="message-source-dot" aria-label={t.messages.newReminder} />}</button>
      <button type="button" aria-label={t.messages.qqMail} aria-pressed={source === "qqMail"} onClick={() => navigate("qqMail")}><MessageSourceIcon source="qqMail" /><span>{t.messages.mail}</span>{mail.enabled && (mail.unreadCount ?? 0) > 0 && <span className="message-source-count">{mailBadgeCount(mail.unreadCount ?? 0)}</span>}</button>
    </div>
  );

  if (source === "qqMail") return <QQMailPanel controller={qqMail} navigation={navigation} />;
  if (source === "wechat") return <WeChatReminderPanel controller={wechatReminders} navigation={navigation} />;
  if (source === "dingtalk") return <DingTalkNotificationPanel controller={dingtalkNotifications} navigation={navigation} />;
  return <QQNotificationPanel controller={qqNotifications} navigation={navigation} />;
}
