// @effect-diagnostics nodeBuiltinImport:off preferSchemaOverJson:off - Subprocess crash fixture with real persistence.
import { join } from "node:path";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as ManagedRuntime from "effect/ManagedRuntime";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { OrchestrationCommand } from "@t3tools/contracts";
import { OrchestrationCommandReceiptRepositoryLive } from "../persistence/Layers/OrchestrationCommandReceipts.ts";
import { OrchestrationEventStoreLive } from "../persistence/Layers/OrchestrationEventStore.ts";
import { makeSqlitePersistenceLive } from "../persistence/Layers/Sqlite.ts";
import * as RepositoryIdentityResolver from "../project/RepositoryIdentityResolver.ts";
import { OrchestrationEngineLive } from "../orchestration/Layers/OrchestrationEngine.ts";
import { OrchestrationProjectionPipelineLive } from "../orchestration/Layers/ProjectionPipeline.ts";
import { OrchestrationProjectionSnapshotQueryLive } from "../orchestration/Layers/ProjectionSnapshotQuery.ts";
import * as ThreadBackgroundLiveness from "../orchestration/ThreadBackgroundLiveness.ts";
import * as ThreadPlanProgress from "../orchestration/ThreadPlanProgress.ts";
import { OrchestrationEngineService } from "../orchestration/Services/OrchestrationEngine.ts";
import { ServerConfig } from "../config.ts";
import { PsiInbox } from "./PsiInbox.ts";
import { PsiBridge, lifecycleId } from "./PsiBridge.ts";
import { drainPsiInbox } from "./PsiProjection.ts";

const [root, mode, binary] = process.argv.slice(2) as [string, string, string];
const inbox = new PsiInbox(join(root, "inbox"));
if (mode === "seed" || mode === "accept") {
  const bridge = new PsiBridge(binary, {
    stateDir: join(root, "rust"),
    ...(mode === "accept" ? { onFrame: (frame) => inbox.accept("psi", frame) } : {}),
  });
  await bridge.hello();
  if (mode === "seed") {
    await bridge.send({ op: "open", request_id: lifecycleId(), session_id: "thread" });
    await bridge.send({
      op: "turn",
      request_id: "origin",
      session_id: "thread",
      turn_id: "turn",
      input: "durable",
    });
  } else
    await bridge.send({
      op: "replay",
      request_id: lifecycleId(),
      session_id: "thread",
      after: inbox.cursor("psi", "thread"),
    });
  await bridge.close();
}
if (mode === "seed" || mode === "project") {
  const layer = OrchestrationEngineLive.pipe(
    Layer.provide(OrchestrationProjectionSnapshotQueryLive),
    Layer.provide(OrchestrationProjectionPipelineLive),
    Layer.provide(ThreadBackgroundLiveness.layer),
    Layer.provide(ThreadPlanProgress.layer),
    Layer.provide(OrchestrationEventStoreLive),
    Layer.provide(OrchestrationCommandReceiptRepositoryLive),
    Layer.provide(RepositoryIdentityResolver.layer),
    Layer.provide(makeSqlitePersistenceLive(join(root, "t3.sqlite"))),
    Layer.provide(ServerConfig.layerTest(root, root)),
    Layer.provide(NodeServices.layer),
  );
  const runtime = ManagedRuntime.make(layer);
  try {
    const engine = await runtime.runPromise(Effect.service(OrchestrationEngineService));
    if (mode === "seed") {
      const createdAt = "2026-09-20T00:00:00.000Z";
      for (const command of [
        {
          type: "project.create",
          commandId: "project",
          projectId: "project",
          title: "Psi",
          workspaceRoot: root,
          createdAt,
        },
        {
          type: "thread.create",
          commandId: "thread",
          threadId: "thread",
          projectId: "project",
          title: "Psi",
          modelSelection: { instanceId: "psi", model: "fixture" },
          runtimeMode: "full-access",
          interactionMode: "default",
          branch: null,
          worktreePath: null,
          createdAt,
        },
      ])
        await runtime.runPromise(
          engine.dispatch(Schema.decodeUnknownSync(OrchestrationCommand)(command)),
        );
    } else await runtime.runPromise(drainPsiInbox(inbox, engine));
  } finally {
    await runtime.dispose();
  }
}
inbox.close();
