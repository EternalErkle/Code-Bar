import { lazy, Suspense, type ComponentProps } from "react";

/// CodeMirror 本体加上六个语言/主题包是首屏 chunk 里最大的一块，
/// 但大多数会话从不打开编辑器。改成按需加载，并在这里统一包好 Suspense，
/// 让调用方的写法保持不变。
const LazyCodeEditorSurface = lazy(() =>
  import("./CodeEditorSurface").then((module) => ({ default: module.CodeEditorSurface }))
);

export function CodeEditorSurface(props: ComponentProps<typeof LazyCodeEditorSurface>) {
  return (
    <Suspense fallback={null}>
      <LazyCodeEditorSurface {...props} />
    </Suspense>
  );
}
