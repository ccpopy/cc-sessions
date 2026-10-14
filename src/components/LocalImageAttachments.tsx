import { useEffect, useState } from "react";
import { ImageOff, Loader2 } from "lucide-react";

import { api } from "@/lib/api";
import type { LocalImageAttachment } from "@/lib/messageAttachments";
import { Dialog, DialogContent, DialogDescription, DialogTitle, DialogTrigger } from "@/components/ui/dialog";

type ImageLoadState =
  | { status: "loading" }
  | { status: "loaded"; src: string }
  | { status: "error"; message: string };

export function LocalImageAttachments({ images }: { images: readonly LocalImageAttachment[] }) {
  if (images.length === 0) return null;

  return (
    <div className="mb-3 flex flex-wrap justify-end gap-2" aria-label="消息图片">
      {images.map((image) => (
        <LocalImageCard key={image.path} image={image} />
      ))}
    </div>
  );
}

function LocalImageCard({ image }: { image: LocalImageAttachment }) {
  const [state, setState] = useState<ImageLoadState>({ status: "loading" });
  const errorTitle =
    state.status === "error" && /not found|找不到|不存在/i.test(state.message)
      ? "本地图片已不存在"
      : "本地图片无法显示";

  useEffect(() => {
    let active = true;
    setState({ status: "loading" });
    void api
      .readPreviewImage(image.path)
      .then((result) => {
        if (active) setState({ status: "loaded", src: result.data_url });
      })
      .catch((error: unknown) => {
        if (!active) return;
        const message = String((error as Error)?.message ?? error);
        setState({ status: "error", message });
      });
    return () => {
      active = false;
    };
  }, [image.path]);

  return (
    <figure className="h-20 w-20 shrink-0 overflow-hidden rounded-lg border border-primary-foreground/20 bg-background text-foreground shadow-sm">
      {state.status === "loading" && (
        <div role="status" className="flex h-full items-center justify-center text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
          <span className="sr-only">正在读取图片：{image.name}</span>
        </div>
      )}
      {state.status === "loaded" && (
        <Dialog>
          <DialogTrigger asChild>
            <button type="button" aria-label={`放大图片：${image.name}`} title={`${image.name}\n${image.path}`}
              className="block h-full w-full cursor-zoom-in rounded-lg bg-muted/30 transition-colors hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring">
              <img
                src={state.src}
                alt={image.name}
                loading="lazy"
                decoding="async"
                className="h-full w-full object-contain"
                onError={() => setState({ status: "error", message: "图片数据无法解码" })}
              />
            </button>
          </DialogTrigger>
          <DialogContent className="max-w-5xl gap-3 p-4 sm:rounded-xl">
            <DialogTitle className="truncate pr-8 text-sm leading-6" title={image.name}>{image.name}</DialogTitle>
            <DialogDescription className="sr-only">图片预览，按 Escape 关闭并返回对话。</DialogDescription>
            <img src={state.src} alt={image.name} className="mx-auto max-h-[calc(100vh-8rem)] max-w-full rounded-md object-contain" />
          </DialogContent>
        </Dialog>
      )}
      {state.status === "error" && (
        <div role="img" aria-label={`${errorTitle}：${image.name}，${state.message}`} title={`${image.path}\n${state.message}`}
          className="flex h-full flex-col items-center justify-center gap-1.5 bg-muted/30 px-1 text-center text-muted-foreground">
          <ImageOff className="h-4 w-4" aria-hidden="true" />
          <span className="text-[10px] leading-tight">{errorTitle}</span>
        </div>
      )}
      <figcaption className="sr-only">{image.name}</figcaption>
    </figure>
  );
}
