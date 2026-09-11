import { join } from "node:path";
import { renameSync, writeFileSync } from "node:fs";
import { ModelRuntime, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { openAICompletionsApi } from "@earendil-works/pi-ai";

// providerCostProvider is the only provider whose charged amount this
// extension republishes. The Runner records the receipt as provider_cost.
const providerCostProvider = "openrouter";

// providerCostFile is the Run-local receipt name. The Runner reads it from the
// Run's PI_CODING_AGENT_DIR before that directory is cleaned up; a missing or
// malformed receipt only means the Run's provider cost is unknown.
const providerCostFile = "provider-cost.json";

// ProviderReceipt is the Run-local provider accounting receipt. The Runner
// records exactly these fields as the Run's provider_cost.
interface ProviderReceipt {
  provider: string;
  cost_usd: number;
  complete: boolean;
}

// ProviderCostReceipt is the Run-scoped accumulator the transport seam uses.
// `begin` opens one model request, `charged` closes it with the provider's own
// amount, and `unbilled` closes a response the provider rejected before any
// generation. A request that is never charged or unbilled stays open, so the
// Run's subtotal remains partial instead of claiming a complete cost.
interface ProviderCostReceipt {
  begin(): void;
  charged(amount: number): void;
  unbilled(): void;
}

// providerCostReceipt accumulates OpenRouter's own charged amounts for the
// Run. `open` counts model requests whose charged amount was never observed:
// an aborted, failed, or usage-less stream leaves the Run's subtotal partial
// instead of silently reporting a complete cost.
function providerCostReceipt(agentDir: string): ProviderCostReceipt {
  const path = join(agentDir, providerCostFile);
  let started = 0;
  let open = 0;
  let costUSD = 0;
  const publish = () => {
    const receipt: ProviderReceipt = {
      provider: providerCostProvider,
      cost_usd: costUSD,
      complete: started > 0 && open === 0,
    };
    const temporary = path + ".tmp";
    try {
      writeFileSync(temporary, JSON.stringify(receipt) + "\n", { mode: 0o600 });
      renameSync(temporary, path);
    } catch {
      // Optional accounting evidence never fails the model request.
    }
  };
  const close = () => {
    if (open > 0) {
      open -= 1;
    }
    publish();
  };
  return {
    begin() {
      started += 1;
      open += 1;
      publish();
    },
    charged(amount) {
      costUSD += amount;
      close();
    },
    unbilled: close,
  };
}

// chargedAmount reads OpenRouter's charged amount from one parsed response
// object. A missing or non-numeric amount is unobserved, never zero.
function chargedAmount(event: unknown): number | undefined {
  if (!event || typeof event !== "object" || !("usage" in event)) {
    return undefined;
  }
  const usage = event.usage;
  if (!usage || typeof usage !== "object" || !("cost" in usage)) {
    return undefined;
  }
  const cost = usage.cost;
  return typeof cost === "number" && Number.isFinite(cost) && cost >= 0 ? cost : undefined;
}

// chargedCost extracts OpenRouter's charged amount from one SSE data line.
// OpenRouter always sends usage in the final chunk; the last observation wins.
function chargedCost(line: string): number | undefined {
  if (!line.startsWith("data:")) {
    return undefined;
  }
  const payload = line.slice(5).trim();
  if (!payload || payload === "[DONE]") {
    return undefined;
  }
  try {
    return chargedAmount(JSON.parse(payload));
  } catch {
    return undefined;
  }
}

// observingFetch passes the provider response through untouched while reading
// the charged amount from the SSE body. The amount reaches the caller only when
// the response finished; a rejected response is not billed, and a stream that
// ends or is cancelled without a final usage chunk leaves the request open.
function observingFetch(
  base: typeof globalThis.fetch,
  receipt: ProviderCostReceipt,
): typeof globalThis.fetch {
  return async (input, init) => {
    const response = await base(input, init);
    if (!response.ok || !response.body) {
      receipt.unbilled();
      return response;
    }
    if (!(response.headers.get("content-type") ?? "").includes("text/event-stream")) {
      // A non-streaming OpenRouter response carries the same usage object.
      try {
        const observed = chargedAmount(await response.clone().json());
        if (observed !== undefined) {
          receipt.charged(observed);
        }
      } catch {
        // An unreadable body leaves the request open, not falsely complete.
      }
      return response;
    }
    const decoder = new TextDecoder();
    let buffer = "";
    let charged: number | undefined;
    const tapped = response.body.pipeThrough(
      new TransformStream<Uint8Array, Uint8Array>({
        transform(chunk, controller) {
          controller.enqueue(chunk);
          buffer += decoder.decode(chunk, { stream: true });
          let index = buffer.indexOf("\n");
          while (index !== -1) {
            const line = buffer.slice(0, index).replace(/\r$/, "");
            buffer = buffer.slice(index + 1);
            const observed = chargedCost(line);
            if (observed !== undefined) {
              charged = observed;
            }
            index = buffer.indexOf("\n");
          }
        },
        flush() {
          // A completed stream without a final usage chunk leaves the request
          // open: the Run's charged amount for it is unknown, not zero.
          if (charged !== undefined) {
            receipt.charged(charged);
          }
        },
      }),
    );
    return new Response(tapped, {
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
    });
  };
}

// Static OpenRouter catalog metadata from /api/v1/models, 2026-09-10.
// Pi 0.84.4 predates this model. Keep its other OpenRouter models and transport;
// Forest's explicit models.json supplies trace metadata and session affinity.
export default async function (pi: ExtensionAPI) {
  // Retain the Runner's explicit overrides without operator state, catalog
  // refresh, or auth checks. The public SDK also works in standalone Pi.
  const agentDir = process.env.PI_CODING_AGENT_DIR;
  if (!agentDir) throw new Error("PI_CODING_AGENT_DIR must name the Forest Run directory");
  const catalog = await ModelRuntime.create({
    modelsPath: join(agentDir, "models.json"),
    refreshOnCreate: false,
    allowModelNetwork: false,
  });
  const receipt = providerCostReceipt(agentDir);
  const completions = openAICompletionsApi();
  const id = "deepseek/deepseek-v4.1-flash";
  pi.registerProvider("openrouter", {
    baseUrl: "https://openrouter.ai/api/v1",
    api: "openai-completions",
    apiKey: "$OPENROUTER_API_KEY",
    models: [
      ...catalog.getModels("openrouter").filter((model) => model.id !== id),
      {
        id,
        name: "DeepSeek: DeepSeek V4.1 Flash",
        reasoning: true,
        thinkingLevelMap: {
          minimal: null,
          low: "low",
          medium: null,
          high: "high",
          xhigh: null,
          max: "max",
        },
        input: ["text", "image"],
        contextWindow: 1048576,
        maxTokens: 384000,
        // Catalog estimates per million tokens, not provider billing authority.
        cost: { input: 0.3, output: 1.2, cacheRead: 0.006, cacheWrite: 0 },
        // Pi applies provider-wide compat before extension model replacement.
        // Preserve the Runner's session-affinity contract on this new entry.
        compat: {
          supportsDeveloperRole: false,
          thinkingFormat: "openrouter",
          sendSessionAffinityHeaders: true,
          sessionAffinityFormat: "openrouter",
        },
      },
    ],
    // Pi 0.84.4's openai-completions adapter recomputes usage.cost from these
    // catalog rates and drops OpenRouter's charged amount. Wrap the transport
    // and republish the provider's own receipt for the Run's Ledger row.
    streamSimple(model, context, options) {
      receipt.begin();
      const baseFetch = options?.fetch ?? globalThis.fetch;
      return completions.streamSimple(model, context, {
        ...options,
        fetch: observingFetch(baseFetch, receipt),
      });
    },
  });
}
