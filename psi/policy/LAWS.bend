import ./Policy.bend as Policy

law cancelled_step:
  for e: Policy.Event
  {Policy.step(Policy.Cancelled{}, e) == Policy.Result{Policy.Cancelled{}, Policy.Deny{}} : Policy.Outcome}

law cancelled_sequence:
  for es: Policy.Events
  {Policy.all_denied(es, Policy.Cancelled{}) == Policy.Deny{} : Policy.Decision}

law cancellation_closes_active:
  {Policy.step(Policy.Active{}, Policy.Cancel{}) == Policy.Result{Policy.Cancelled{}, Policy.Deny{}} : Policy.Outcome}

law finished_sequence:
  for es: Policy.Events
  {Policy.all_denied(es, Policy.Finished{}) == Policy.Deny{} : Policy.Decision}
