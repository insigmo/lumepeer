// Vite's `?raw` suffix, which hands a file over as a string.
//
// Used by `view-window.test.ts` to assert on `view.html` itself: two of the
// view window's rules live in that file's `<style>` — `#chat-panel[hidden]`
// and `#cursor[hidden]` — and an id selector beats the browser's own
// `[hidden] { display: none }`, so nothing in TypeScript can stand in for
// them.
declare module '*.html?raw' {
  const content: string;
  export default content;
}
