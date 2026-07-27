import os
import unittest

from nanocodex import Nanocodex


class BindingTests(unittest.TestCase):
    def test_constructs_owned_handle_and_event_stream_without_exposing_secret(self) -> None:
        secret = "private-test-value"
        agent, events = Nanocodex(
            secret, thinking="none", reasoning_mode="pro"
        )
        self.assertNotIn(secret, repr(agent))
        self.assertTrue(callable(agent.prompt))
        self.assertTrue(callable(agent.spawn))
        self.assertTrue(callable(agent.fork))
        self.assertTrue(callable(agent.fork_from))
        agent.set_thinking("high")
        agent.set_fast_mode(True)
        self.assertTrue(callable(events.recv_json))

    def test_configuration_errors_cross_the_boundary(self) -> None:
        with self.assertRaisesRegex(ValueError, "expected none"):
            Nanocodex("test-key", thinking="impossible")

        with self.assertRaisesRegex(ValueError, "expected standard or pro"):
            Nanocodex("test-key", reasoning_mode="impossible")

        agent, _ = Nanocodex("test-key")
        with self.assertRaisesRegex(ValueError, "expected none"):
            agent.set_thinking("impossible")

        with self.assertRaisesRegex(RuntimeError, "OpenAI credentials are empty"):
            Nanocodex("")

    def test_spawn_returns_independent_agent_without_network(self) -> None:
        agent, _ = Nanocodex("test-key", thinking="none")
        child, child_events = agent.spawn()
        self.assertTrue(callable(child.prompt))
        self.assertTrue(callable(child_events.recv_json))
        self.assertIsNot(agent, child)

    def test_fork_before_safe_boundary_is_typed(self) -> None:
        agent, _ = Nanocodex("test-key", thinking="none")
        with self.assertRaises(RuntimeError):
            agent.fork()

    def test_empty_steer_is_rejected(self) -> None:
        agent, _ = Nanocodex("test-key", thinking="none")
        turn = agent.prompt("queued for steer rejection")
        with self.assertRaisesRegex(RuntimeError, "steer instruction must not be empty"):
            turn.steer("")
        turn.cancel()

    def test_fork_from_before_result_is_rejected(self) -> None:
        agent, _ = Nanocodex("test-key", thinking="none")
        turn = agent.prompt("incomplete")
        with self.assertRaisesRegex(RuntimeError, "turn has not completed"):
            agent.fork_from(turn)
        with self.assertRaisesRegex(RuntimeError, "turn has not completed"):
            turn.usage()
        turn.cancel()

    @unittest.skipUnless(os.environ.get("OPENAI_API_KEY"), "live API key not configured")
    def test_live_follow_on_prompting(self) -> None:
        agent, _ = Nanocodex(os.environ["OPENAI_API_KEY"], thinking="low")
        first = agent.prompt("Remember the token PYO3_LIVE. Reply with OK.")
        self.assertIn("OK", first.result())
        second = agent.prompt("What token did I ask you to remember? Reply with only it.")
        self.assertEqual(second.result().strip(), "PYO3_LIVE")


if __name__ == "__main__":
    unittest.main()
