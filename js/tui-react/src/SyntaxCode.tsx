import { memo, useEffect, useState } from "react";
import type { SpecialLanguage, ThemeRegistrationRaw } from "shiki/core";

const MAX_HIGHLIGHT_CHARS = 50_000;
const MAX_CACHE_ENTRIES = 256;
const MAX_CACHE_BYTES = 16 * 1024 * 1024;

type HighlightCacheEntry = {
  promise: Promise<string>;
  retainedBytes: number;
};

const htmlCache = new Map<string, HighlightCacheEntry>();
let htmlCacheBytes = 0;

const languageLoaders = {
  bash: () => import("@shikijs/langs/bash"),
  c: () => import("@shikijs/langs/c"),
  cpp: () => import("@shikijs/langs/cpp"),
  css: () => import("@shikijs/langs/css"),
  diff: () => import("@shikijs/langs/diff"),
  dockerfile: () => import("@shikijs/langs/dockerfile"),
  go: () => import("@shikijs/langs/go"),
  html: () => import("@shikijs/langs/html"),
  javascript: () => import("@shikijs/langs/javascript"),
  json: () => import("@shikijs/langs/json"),
  jsx: () => import("@shikijs/langs/jsx"),
  markdown: () => import("@shikijs/langs/markdown"),
  python: () => import("@shikijs/langs/python"),
  rust: () => import("@shikijs/langs/rust"),
  shellscript: () => import("@shikijs/langs/shellscript"),
  sql: () => import("@shikijs/langs/sql"),
  toml: () => import("@shikijs/langs/toml"),
  tsx: () => import("@shikijs/langs/tsx"),
  typescript: () => import("@shikijs/langs/typescript"),
  xml: () => import("@shikijs/langs/xml"),
  yaml: () => import("@shikijs/langs/yaml"),
  zig: () => import("@shikijs/langs/zig"),
} as const;

type SupportedLanguage = keyof typeof languageLoaders;
type SyntaxLanguage = SupportedLanguage | SpecialLanguage;

const languageAliases: Record<string, SupportedLanguage> = {
  bash: "bash",
  c: "c",
  cpp: "cpp",
  css: "css",
  diff: "diff",
  dockerfile: "dockerfile",
  go: "go",
  html: "html",
  javascript: "javascript",
  js: "javascript",
  json: "json",
  jsx: "jsx",
  markdown: "markdown",
  md: "markdown",
  python: "python",
  py: "python",
  rs: "rust",
  rust: "rust",
  sh: "shellscript",
  shell: "shellscript",
  sql: "sql",
  toml: "toml",
  ts: "typescript",
  tsx: "tsx",
  typescript: "typescript",
  xml: "xml",
  yaml: "yaml",
  yml: "yaml",
  zig: "zig",
};

export const SyntaxCode = memo(function SyntaxCode({
  code,
  language,
  streaming = false,
  tree = false,
}: {
  code: string;
  language?: string;
  streaming?: boolean;
  tree?: boolean;
}) {
  const [html, setHtml] = useState<string>();
  const resolved = resolveLanguage(language);

  useEffect(() => {
    setHtml(undefined);
    // Re-highlighting every token swaps colored HTML for the plain fallback on
    // each streaming frame. Keep the live block stable, then color it once the
    // response seals.
    if (streaming || !code || code.length > MAX_HIGHLIGHT_CHARS) return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void highlightedHtml(code, resolved).then((value) => {
        if (!cancelled) setHtml(value);
      }).catch(() => {
        // Plain text remains visible when a grammar cannot be loaded.
      });
    }, 48);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [code, resolved, streaming]);

  const className = `tui-syntax${tree ? " is-tree" : ""}`;
  if (!html) {
    const lines = tree ? code.split("\n") : [];
    return tree ? (
      <pre className={`${className} is-plain`}><code>{lines.map((line, index) => (
        <span className="line" key={index}>{line || "\u00a0"}{index + 1 < lines.length ? "\n" : null}</span>
      ))}</code></pre>
    ) : <pre className={`${className} is-plain`}><code>{code}</code></pre>;
  }
  return <div className={className} dangerouslySetInnerHTML={{ __html: html }} />;
});

function resolveLanguage(language?: string): SyntaxLanguage {
  return languageAliases[(language ?? "").toLowerCase()] ?? "text";
}

function highlightedHtml(code: string, language: SyntaxLanguage): Promise<string> {
  const key = `${language}\0${code}`;
  const cached = htmlCache.get(key);
  if (cached) {
    htmlCache.delete(key);
    htmlCache.set(key, cached);
    return cached.promise;
  }

  const entry: HighlightCacheEntry = {
    promise: Promise.resolve(""),
    retainedBytes: estimatedStringBytes(key),
  };
  entry.promise = renderHighlightedHtml(code, language)
    .then((html) => {
      if (htmlCache.get(key) === entry) {
        const renderedBytes = estimatedStringBytes(html);
        entry.retainedBytes += renderedBytes;
        htmlCacheBytes += renderedBytes;
        trimHtmlCache();
      }
      return html;
    })
    .catch((error) => {
      removeHtmlCacheEntry(key, entry);
      throw error;
    });
  htmlCache.set(key, entry);
  htmlCacheBytes += entry.retainedBytes;
  trimHtmlCache();
  return entry.promise;
}

function estimatedStringBytes(value: string): number {
  return value.length * 2;
}

function removeHtmlCacheEntry(key: string, entry: HighlightCacheEntry) {
  if (htmlCache.get(key) !== entry) return;
  htmlCache.delete(key);
  htmlCacheBytes = Math.max(0, htmlCacheBytes - entry.retainedBytes);
}

function trimHtmlCache() {
  while (
    htmlCache.size > MAX_CACHE_ENTRIES ||
    htmlCacheBytes > MAX_CACHE_BYTES
  ) {
    const oldestKey = htmlCache.keys().next().value;
    if (oldestKey === undefined) return;
    const oldest = htmlCache.get(oldestKey);
    if (oldest === undefined) return;
    removeHtmlCacheEntry(oldestKey, oldest);
  }
}

async function renderHighlightedHtml(code: string, language: SyntaxLanguage): Promise<string> {
  const codeToHtml = await loadCodeToHtml();
  return codeToHtml(code, {
    lang: language,
    themes: {
      light: "pierre-light",
      dark: "pierre-dark",
    },
    defaultColor: false,
  });
}

let codeToHtmlPromise: ReturnType<typeof createCodeToHtml> | undefined;

function loadCodeToHtml(): ReturnType<typeof createCodeToHtml> {
  codeToHtmlPromise ??= createCodeToHtml();
  return codeToHtmlPromise;
}

async function createCodeToHtml() {
  const [{ createBundledHighlighter, createSingletonShorthands }, { createJavaScriptRegexEngine }] = await Promise.all([
    import("shiki/core"),
    import("shiki/engine/javascript"),
  ]);
  const createHighlighter = createBundledHighlighter({
    langs: languageLoaders,
    themes: {
      "pierre-light": async () => ({
        default: (await import("@pierre/theme/pierre-light")).default as unknown as ThemeRegistrationRaw,
      }),
      "pierre-dark": async () => ({
        default: (await import("@pierre/theme/pierre-dark")).default as unknown as ThemeRegistrationRaw,
      }),
    },
    engine: createJavaScriptRegexEngine,
  });
  return createSingletonShorthands(createHighlighter).codeToHtml;
}
