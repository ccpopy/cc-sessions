import { useEffect, useState } from "react";
import { ImageIcon, ImageOff, Loader2 } from "lucide-react";

import { api } from "@/lib/api";
import type { LocalImageAttachment } from "@/lib/messageAttachments";

type ImageLoadState =
  | { status: "loading" }
  | { status: "loaded"; src: string }
  | { status: "error"; message: string };

export function LocalImageAttachments({ images }: { images: readonly LocalImageAttachment[] }) {
  if (images.length === 0) return null;

  return (
    <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-2" aria-label="消息图片">
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
    <figure className="overflow-hidden rounded-lg border border-primary-foreground/20 bg-background text-foreground shadow-sm">
      {state.status === "loading" && (
        <div className="flex min-h-32 items-center justify-center gap-2 px-4 py-8 text-xs text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          正在读取图片…
        </div>
      )}
      {state.status === "loaded" && (
        <img
          src={state.src}
          alt={image.name}
          title={image.path}
          loading="lazy"
          decoding="async"
          className="max-h-80 w-full bg-muted/30 object-contain"
          onError={() => setState({ status: "error", message: "图片数据无法解码" })}
        />
      )}
      {state.status === "error" && (
        <div className="flex min-h-32 flex-col items-center justify-center gap-2 bg-destructive/5 px-4 py-6 text-center">
          <ImageOff className="h-5 w-5 text-destructive" />
          <span className="text-xs font-medium text-destructive">{errorTitle}</span>
          <span className="max-w-full break-all text-[11px] leading-relaxed text-muted-foreground">
            {state.message}
          </span>
        </div>
      )}
      <figcaption className="flex min-w-0 items-center gap-1.5 border-t px-2.5 py-1.5 text-[11px] text-muted-foreground">
        <ImageIcon className="h-3 w-3 shrink-0" />
        <span className="truncate" title={image.path}>
          {image.name}
        </span>
      </figcaption>
    </figure>
  );
}
