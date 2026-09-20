type State is Data:
  Active{}
  Cancelled{}
  Finished{}

type Event is Data:
  Request{}
  Cancel{}
  Finish{}
  Late{}

type Decision is Data:
  Deny{}
  Permit{}

type Outcome is Data:
  Result{state: State, decision: Decision}

def step(s: State, e: Event) -> Outcome:
  match s:
    case Cancelled{}: Result{Cancelled{}, Deny{}}
    case Finished{}: Result{Finished{}, Deny{}}
    case Active{}:
      match e:
        case Request{}: Result{Active{}, Permit{}}
        case Cancel{}: Result{Cancelled{}, Deny{}}
        case Finish{}: Result{Finished{}, Deny{}}
        case Late{}: Result{Active{}, Deny{}}

type Events is Data:
  End{}
  More{event: Event, rest: Events}

def next(out: Outcome) -> State:
  match out:
    case Result{s, d}: s

def decision(out: Outcome) -> Decision:
  match out:
    case Result{s, d}: d

def combine(d: Decision, tail: Decision) -> Decision:
  match d:
    case Permit{}: Permit{}
    case Deny{}: tail

def all_denied(es: Events, +s: State) -> Decision:
  match es:
    case End{}: Deny{}
    case More{+e, rest}: combine(decision(step(s, e)), all_denied(rest, next(step(s, e))))
