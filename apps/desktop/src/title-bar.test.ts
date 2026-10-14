// The main window's title strip: Windows-style controls of its own on
// Windows and Linux, and none on macOS, where the native traffic lights are
// the window's controls (tauri.macos.conf.json).

import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { titleBar } from './title-bar';

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

describe('title bar', () => {
  it('draws minimize, maximize and close off a Mac', () => {
    render(titleBar('en', false), container);
    const labels = [...container.querySelectorAll('.title-bar-controls button')].map((button) =>
      button.getAttribute('aria-label'),
    );
    expect(labels).toHaveLength(3);
    expect(container.querySelector('.close-btn')).not.toBeNull();
  });

  it('draws no controls of its own on a Mac, only the drag surface', () => {
    render(titleBar('en', true), container);
    expect(container.querySelector('.title-bar-controls')).toBeNull();
    expect(container.querySelector('button')).toBeNull();
    expect(container.querySelector('.title-bar[data-tauri-drag-region]')).not.toBeNull();
  });
});
