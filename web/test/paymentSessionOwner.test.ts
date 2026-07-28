import assert from "node:assert/strict";
import { test } from "node:test";

import { createPaymentSessionOwner } from "../src/paymentSessionOwner.ts";

test("MPP session replacement and failed Agent creation close caller resources", async () => {
  const owner = createPaymentSessionOwner<FakePaymentSession>();
  const first = session("first");
  const failed = session("failed");
  const order: string[] = [];

  await owner.open(
    async () => first,
    async () => {
      order.push("opened:first");
      return "agent";
    },
  );
  await assert.rejects(
    owner.open(
      async () => {
        order.push("created:failed");
        return failed;
      },
      async () => {
        throw new Error("Agent.create failed");
      },
    ),
    /Agent\.create failed/,
  );
  await owner.clear();

  assert.deepEqual(order, [
    "opened:first",
    "closed:first",
    "created:failed",
    "closed:failed",
  ]);
  assert.equal(first.closes, 1);
  assert.equal(failed.closes, 1);

  function session(name: string): FakePaymentSession {
    const value: FakePaymentSession = {
      closes: 0,
      mpp: {
        close() {
          order.push(`closed:${name}`);
          value.closes += 1;
        },
      },
      name,
    };
    return value;
  }
});

test("clearing before pre-session setup failure closes the prior manager", async () => {
  const owner = createPaymentSessionOwner<FakePaymentSession>();
  const prior = simpleSession();

  await owner.open(async () => prior, async () => "agent");
  await owner.clear();
  await assert.rejects(
    async () => {
      throw new Error("Tempo module import failed");
    },
    /module import failed/,
  );

  assert.equal(prior.closes, 1);
});

type FakePaymentSession = {
  name: string;
  closes: number;
  mpp: { close(): void };
};

function simpleSession(): FakePaymentSession {
  const value: FakePaymentSession = {
    closes: 0,
    mpp: {
      close() {
        value.closes += 1;
      },
    },
    name: "prior",
  };
  return value;
}
