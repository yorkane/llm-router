"use client";

import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import {
  load_harmony_encoding,
  type JsHarmonyEncoding,
  initSync as initHarmony,
  JsStreamableParser,
} from "@openai/harmony";
import {
  QueryClient,
  QueryClientProvider,
  useQuery,
} from "@tanstack/react-query";
import { ChevronsUpDown, PlusIcon } from "lucide-react";
import dynamic from "next/dynamic";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";

const SPECIAL_TOKENS_VALUES = {
  "<|startoftext|>": 199998,
  "<|endoftext|>": 199999,
  "<|return|>": 200002,
  "<|constrain|>": 200003,
  "<|channel|>": 200005,
  "<|start|>": 200006,
  "<|end|>": 200007,
  "<|message|>": 200008,
  "<|call|>": 200012,
};

// Map from token id to special token string
const TOKEN_ID_TO_SPECIAL_TOKEN: Record<number, string> = Object.entries(
  SPECIAL_TOKENS_VALUES
).reduce((acc, [key, value]) => {
  acc[value] = key;
  return acc;
}, {} as Record<number, string>);

const TOKEN_COLORS = [
  "bg-red-300/30 data-[active=true]:bg-red-300",
  "bg-teal-300/30 data-[active=true]:bg-teal-300",
  "bg-amber-300/30 data-[active=true]:bg-amber-300",
  "bg-indigo-300/30 data-[active=true]:bg-indigo-300",
  "bg-pink-300/30 data-[active=true]:bg-pink-300",
  "bg-green-300/30 data-[active=true]:bg-green-300",
  "bg-orange-300/30 data-[active=true]:bg-orange-300",
  "bg-purple-300/30 data-[active=true]:bg-purple-300",
  "bg-yellow-300/30 data-[active=true]:bg-yellow-300",
  "bg-cyan-300/30 data-[active=true]:bg-cyan-300",
  "bg-lime-300/30 data-[active=true]:bg-lime-300",
  "bg-violet-300/30 data-[active=true]:bg-violet-300",
  "bg-emerald-300/30 data-[active=true]:bg-emerald-300",
  "bg-rose-300/30 data-[active=true]:bg-rose-300",
  "bg-sky-300/30 data-[active=true]:bg-sky-300",
  "bg-fuchsia-300/30 data-[active=true]:bg-fuchsia-300",
  "bg-blue-300/30 data-[active=true]:bg-blue-300",
];

const isSpecialToken = (token: string | number) => {
  if (typeof token === "number") {
    return Object.values(SPECIAL_TOKENS_VALUES).includes(token);
  }
  return Object.keys(SPECIAL_TOKENS_VALUES).includes(token);
};

const getSpecialTokenDescription = (token: number | string) => {
  const tokenName =
    typeof token === "number" ? TOKEN_ID_TO_SPECIAL_TOKEN[token] : token;

  switch (tokenName) {
    case "<|return|>":
      return "Stop token. Indicates the model is done sampling its response.";
    case "<|constrain|>":
      return "Part of the message header to indicate the content type of the message. Primarily used for function calls to constrain the response to JSON.";
    case "<|channel|>":
      return "Part of the message header to indicate the channel the model's response is intended for.";
    case "<|start|>":
      return "Indicates the start of a new message. Tokens following this token are part of the header.";
    case "<|end|>":
      return "Indicates the end of a message. The next token is the start of the next message. This should not end sampling.";
    case "<|message|>":
      return "Completes the message header. Tokens following this token are part of the message content.";
    case "<|call|>":
      return "Stop token. Indicates the model is done sampling its response and is ready to perform the function call.";
    default:
      return `Unknown special token: ${tokenName}`;
  }
};

function WrappedSpecialToken({
  token,
  children,
}: {
  token: string | number;
  children: React.ReactNode;
}) {
  if (isSpecialToken(token)) {
    return (
      <Tooltip>
        <TooltipTrigger>{children}</TooltipTrigger>
        <TooltipContent>{getSpecialTokenDescription(token)}</TooltipContent>
      </Tooltip>
    );
  }
  return children;
}

function HighlightedTokens({
  tokens,
  highlightedIndex,
  onHover,
  wrapWhitespace,
  highlightTokens,
  editable,
  onEdit,
}: {
  tokens: (number | string)[];
  highlightedIndex?: number;
  onHover?: (index?: number) => void;
  wrapWhitespace?: boolean;
  highlightTokens?: boolean;
  editable?: boolean;
  onEdit?: (text: string | number[]) => void;
}) {
  const areTokenIds = tokens.every((token) => typeof token === "number");
  const [isEditing, setIsEditing] = useState(false);
  const [text, setText] = useState(
    areTokenIds ? JSON.stringify(tokens) : tokens.join("")
  );
  const editorRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (editorRef.current) {
      editorRef.current.focus();
      editorRef.current.select();
    }
  }, [editorRef.current, isEditing]);

  useEffect(() => {
    if (editable) {
      setText(areTokenIds ? JSON.stringify(tokens) : tokens.join(""));
    }
  }, [tokens]);

  if (isEditing) {
    return (
      <div className="aspect-video rounded-xl p-4 font-mono bg-gray-50 border border-gray-200">
        <textarea
          className="w-full h-full font-mono text-sm"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onBlur={() => {
            setIsEditing(false);
            onEdit?.(text);
          }}
          ref={editorRef}
        />
      </div>
    );
  }

  return (
    <div
      className="aspect-video rounded-xl p-4 font-mono bg-gray-50 border border-gray-200"
      onClick={() => editable && setIsEditing(true)}
    >
      <pre
        className={cn(
          "text-sm max-h-40 overflow-y-auto min-h-full",
          wrapWhitespace ? "whitespace-pre-wrap break-all" : "whitespace-pre"
        )}
      >
        {areTokenIds && "["}
        {tokens.map((token, index) => (
          <WrappedSpecialToken token={token} key={index}>
            <code
              className={cn(
                // highlightTokens ? "rounded-[3px] px-1 ml-1 mr-1" : "",
                areTokenIds ? "ml-1 mr-1" : "",
                // highlightedIndex === index ? "bg-blue-500 text-white" : "",
                isSpecialToken(token) && !areTokenIds
                  ? "font-semibold"
                  : isSpecialToken(token)
                  ? "underline decoration-dotted"
                  : "",
                highlightTokens ? TOKEN_COLORS[index % TOKEN_COLORS.length] : ""
              )}
              onMouseEnter={() => onHover?.(index)}
              onMouseLeave={() => onHover?.(undefined)}
              data-active={highlightedIndex === index}
            >
              {token}
            </code>
            {areTokenIds && index < tokens.length - 1 && ","}
          </WrappedSpecialToken>
        ))}
        {areTokenIds && "]"}
      </pre>
    </div>
  );
}

type Message = {
  role: "assistant" | "user" | "system" | "developer" | "tool";
  channel?: string;
  recipient?: string;
  content_type?: string;
  content: {
    type: "text";
    text: string;
  }[];
};

function MessageItem({ message }: { message: Message }) {
  return (
    <div className="text-sm border rounded-md p-1">
      <div className="border border-dashed border-gray-300 rounded-md">
        <Collapsible defaultOpen>
          <div className="flex items-center justify-between">
            <span className="bg-gray-300 text-xs rounded-tl-sm rounded-br-sm px-2 py-1 -ml-[1px]">
              header
            </span>
            <CollapsibleTrigger asChild>
              <Button variant="ghost" size="icon" className="size-4 px-4">
                <ChevronsUpDown />
                <span className="sr-only">Toggle</span>
              </Button>
            </CollapsibleTrigger>
          </div>
          <CollapsibleContent>
            <div className="p-2 flex flex-col gap-1 mt-1">
              <dl className="text-xs flex gap-1">
                <dt>role:</dt>
                <dd className="font-semibold bg-blue-300/50 px-1 rounded-sm">
                  {message.role}
                </dd>
              </dl>
              {message.channel && (
                <dl className="text-xs flex gap-1">
                  <dt>content:</dt>
                  <dd className="font-semibold bg-blue-300/50 px-1 rounded-sm">
                    {message.channel}
                  </dd>
                </dl>
              )}
              {message.recipient && (
                <dl className="text-xs flex gap-1">
                  <dt>recipient:</dt>
                  <dd className="font-semibold bg-blue-300/50 px-1 rounded-sm">
                    {message.recipient}
                  </dd>
                </dl>
              )}
              {message.content_type && (
                <dl className="text-xs flex gap-1">
                  <dt>content_type:</dt>
                  <dd className="font-semibold bg-blue-300/50 px-1 rounded-sm">
                    {message.content_type}
                  </dd>
                </dl>
              )}
            </div>
          </CollapsibleContent>
        </Collapsible>
      </div>
      <div className="text-sm flex flex-col gap-4 mt-3 mb-1">
        {message.content.map((content: any, idx: number) => (
          <pre
            className="bg-gray-100/50 p-4 rounded-md text-xs overflow-x-scroll"
            key={idx}
          >
            {content.text}
          </pre>
        ))}
      </div>
    </div>
  );
}

function useHarmony() {
  const [harmony, setHarmony] = useState<JsHarmonyEncoding | null>(null);

  useEffect(() => {
    const fetchHarmony = async () => {
      const response = await fetch("/openai_harmony_bg.wasm");
      const wasm = await response.arrayBuffer();
      initHarmony({ module: wasm });
      console.log("wasm", wasm);
      const encoding = await load_harmony_encoding(
        "HarmonyGptOss",
        typeof window !== "undefined" ? `${window.location.origin}/` : "/"
      );
      setHarmony(encoding);
    };
    fetchHarmony();
  }, []);

  return harmony;
}

function useTokens(text: string | undefined) {
  const [highlightedIndex, setHighlightedIndex] = useState<number | undefined>(
    undefined
  );
  const encoding = useHarmony();

  const [tokenData, setTokenData] = useState<{
    tokens: number[];
    tokenTexts: string[];
    messages: Message[];
  }>({ tokens: [], tokenTexts: [], messages: [] });

  useEffect(() => {
    if (!text || !encoding) return;
    const tokens = Array.from(encoding.encode(text, encoding.specialTokens()));
    // @ts-ignore
    const tokenTexts = tokens.map((t) => encoding.decodeUtf8([t]));
    const parser = new JsStreamableParser(encoding, "user");
    for (const token of tokens) {
      parser.process(token);
    }
    const messages = JSON.parse(parser.messages).map((m: any) => ({
      ...m,
      content:
        typeof m.content === "string"
          ? [{ type: "text", text: m.content }]
          : m.content,
    }));
    console.log("messages", typeof messages, messages);

    setTokenData({
      tokens: Array.from(tokens),
      tokenTexts: Array.from(tokenTexts),
      messages: messages,
    });
  }, [text, encoding]);

  return {
    ...tokenData,
    highlightedIndex,
    setHighlightedIndex,
  };
}

function App() {
  const [text, setText] = useState(
    `<|start|>user<|message|>What is the weather in SF?<|end|><|start|>assistant<|channel|>analysis<|message|>User asks: “What is the weather in SF?” We need to use lookup_weather tool.<|end|><|start|>assistant to=functions.lookup_weather<|channel|>commentary <|constrain|>json<|message|>{"location": "San Francisco"}<|end|><|start|>assistant`
  );
  const {
    tokens,
    tokenTexts,
    highlightedIndex,
    setHighlightedIndex,
    messages,
  } = useTokens(text);

  const [wrapWhitespace, setWrapWhitespace] = useState(true);
  const [highlightTokens, setHighlightTokens] = useState(true);

  return (
    <div className="p-8 pt-6 max-w-screen-xl mx-auto">
      <h1 className="text-3xl font-bold mb-4 tracking-tight">
        OpenAI harmony response format
      </h1>
      <p className="text-sm text-gray-500 mb-4">
        The{" "}
        <a
          href="https://github.com/openai/open-models"
          className="text-blue-500 focus:underline hover:underline"
          target="_blank"
        >
          gpt-oss
        </a>{" "}
        models were trained on the harmony response format for defining
        conversation structures, generating reasoning output and structuring
        function calls. If you are not using gpt-oss directly but through an API
        or a provider like{" "}
        <a
          href="https://ollama.ai"
          className="text-blue-500"
          target="_blank"
          rel="noopener noreferrer"
        >
          Ollama
        </a>
        , you will not have to be concerned about this as your inference
        solution will handle the formatting.
      </p>
      <p className="text-sm text-gray-500 mb-8">
        If you are trying to count input tokens for text to an OpenAI model,
        visit the{" "}
        <a
          href="https://platform.openai.com/tokenizer"
          className="text-blue-500 focus:underline hover:underline"
          target="_blank"
        >
          OpenAI Tokenizer
        </a>{" "}
        instead.
      </p>
      <div className="grid md:grid-cols-2 gap-4 mt-8">
        <div>
          <h3 className="text-sm font-semibold text-gray-700 mb-2">Messages</h3>
          <div className="flex flex-col gap-4">
            {messages.map((message: Message, idx: number) => (
              <MessageItem key={idx} message={message} />
            ))}
            {/* <Button
              variant="outline"
              className="border border-dashed border-gray-300 rounded-md text-center py-8 text-sm text-gray-500 cursor-pointer"
            >
              <PlusIcon /> Add new message
            </Button> */}
          </div>
        </div>
        <div className="flex flex-col gap-4">
          <div>
            <h3 className="text-sm font-semibold text-gray-700 mb-2">
              Decoded tokens{" "}
              <span className="text-xs text-gray-500 font-light">
                {/* (click to edit) */}
              </span>
            </h3>
            <HighlightedTokens
              tokens={tokenTexts}
              highlightedIndex={highlightedIndex}
              onHover={setHighlightedIndex}
              wrapWhitespace={wrapWhitespace}
              highlightTokens={highlightTokens}
              editable={true}
              onEdit={(text) => typeof text === "string" && setText(text)}
            />
          </div>
          <div>
            <h3 className="text-sm font-semibold text-gray-700 mb-2">
              Token IDs
            </h3>
            <HighlightedTokens
              tokens={tokens}
              highlightedIndex={highlightedIndex}
              onHover={setHighlightedIndex}
              wrapWhitespace={wrapWhitespace}
              highlightTokens={highlightTokens}
            />
          </div>
          {/* <div className="flex flex-col md:flex-row gap-2">
            <div className="flex items-center space-x-2">
              <Switch
                id="wrap-whitespace"
                checked={wrapWhitespace}
                onCheckedChange={() => setWrapWhitespace((val) => !val)}
              />
              <Label htmlFor="wrap-whitespace">Wrap Whitespace</Label>
            </div>
            <div className="flex items-center space-x-2">
              <Switch
                id="highlight-tokens"
                checked={highlightTokens}
                onCheckedChange={() => setHighlightTokens((val) => !val)}
              />
              <Label htmlFor="highlight-tokens">Highlight Tokens</Label>
            </div>
          </div> */}
        </div>
      </div>
    </div>
  );
}

const HarmonyDemo = dynamic(() => Promise.resolve(App), {
  ssr: false,
});

export default HarmonyDemo;
