// The session agent's indicator (ADR 0085 §3b).
//
// A machine hosted by `LumepeerHost` has no Lumepeer window on it: the actor,
// the consent dialog and the revoke all live in a `LocalSystem` service that
// draws nothing. This strip is the whole of what the person at that machine
// sees, which is why it says one thing and offers nothing.
//
// It is deliberately the smallest page in this application. It calls no IPC —
// there is no actor in this process to call — reads no state and polls
// nothing, so there is no failure mode in which it renders empty. What it
// shows is a fact the agent established before this window existed: the window
// is only ever open while the host has an attachment, and the host raises it
// before it starts capture, so the strip being on screen *is* the statement.

import { detectLocale, dirOf, t, type Locale } from './i18n';

const root = document.querySelector<HTMLElement>('#agentbar');
const locale: Locale = detectLocale(navigator);

document.documentElement.lang = locale;
document.documentElement.dir = dirOf(locale);

if (root) {
  const mark = document.createElement('span');
  mark.className = 'mark';
  // Decorative: the sentence beside it already says everything, and a screen
  // reader announcing "bullet" before it would be noise.
  mark.setAttribute('aria-hidden', 'true');

  const text = document.createElement('span');
  text.className = 'text';
  text.textContent = t(locale, 'agent.indicator');

  // `status`, not `alert`: it is true for as long as the window is up rather
  // than an event that just happened, and an alert would interrupt whatever
  // the person at this machine is doing every time the window is rebuilt.
  root.setAttribute('role', 'status');
  root.append(mark, text);
  root.title = text.textContent;
}
