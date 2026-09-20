import ./LAWS.bend as Laws

def Laws.cancelled_step(e):
  {==}

def Laws.cancelled_sequence(es):
  match es:
    case Policy.End{}: {==}
    case Policy.More{e, rest}: Laws.cancelled_sequence(rest)

def Laws.cancellation_closes_active():
  {==}

def Laws.finished_sequence(es):
  match es:
    case Policy.End{}: {==}
    case Policy.More{e, rest}: Laws.finished_sequence(rest)
