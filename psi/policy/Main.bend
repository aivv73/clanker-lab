# B1 CPU process adapter; IO and serialization are outside the pure proof boundary.
import Base
import ./Policy.bend as P

def state_choice(a: Bool, b: Bool, c: Bool) -> Maybe<&2, P.State>:
  match a b c:
    case True{} _ _: Some{P.Active{}}
    case _ True{} _: Some{P.Cancelled{}}
    case _ _ True{}: Some{P.Finished{}}
    case _ _ _: None{}

def parse_state(+s: String) -> Maybe<&2, P.State>:
  state_choice(String.eq(s, "Active"), String.eq(s, "Cancelled"), String.eq(s, "Finished"))

def event_choice(a: Bool, b: Bool, c: Bool, d: Bool) -> Maybe<&2, P.Event>:
  match a b c d:
    case True{} _ _ _: Some{P.Request{}}
    case _ True{} _ _: Some{P.Cancel{}}
    case _ _ True{} _: Some{P.Finish{}}
    case _ _ _ True{}: Some{P.Late{}}
    case _ _ _ _: None{}

def parse_event(+s: String) -> Maybe<&2, P.Event>:
  event_choice(String.eq(s, "Request"), String.eq(s, "Cancel"), String.eq(s, "Finish"), String.eq(s, "Late"))

def show_state(s: P.State) -> String:
  match s:
    case P.Active{}: "Active"
    case P.Cancelled{}: "Cancelled"
    case P.Finished{}: "Finished"

def show_decision(d: P.Decision) -> String:
  match d:
    case P.Permit{}: "1"
    case P.Deny{}: "0"

def output(req: String, turn: String, action: String, epoch: String, out: P.Outcome) -> IO(Unit):
  match out:
    case P.Result{next, decision}:
      IO.print("B1\t" ++ req ++ "\t" ++ turn ++ "\t" ++ action ++ "\t" ++ epoch ++ "\t" ++ show_state(next) ++ "\t" ++ show_decision(decision))

def checked(req: String, turn: String, action: String, epoch: String, s: Maybe<&2, P.State>, e: Maybe<&2, P.Event>) -> IO(Unit):
  match s e:
    case Some{state} Some{event}: output(req, turn, action, epoch, P.step(state, event))
    case _ _: IO.die(Unit, 2, "invalid state or event")

def respond(req: String, turn: String, action: String, epoch: String, s: String, e: String) -> IO(Unit):
  checked(req, turn, action, epoch, parse_state(s), parse_event(e))

def handle(xs: List<String>) -> IO(Unit):
  match xs:
    case Con{req, Con{turn, Con{action, Con{epoch, Con{s, Con{e, Nil{}}}}}}}:
      respond(req, turn, action, epoch, s, e)
    case _: IO.die(Unit, 2, "expected request turn action epoch state event")

def main() -> IO(Unit):
  do IO<Unit>:
    args : List<String> <- IO.args()
    handle(args)
