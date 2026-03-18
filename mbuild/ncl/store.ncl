{
  return = fun x => 'Return x,

  bind = fun action cont => 'Bind { include [action, cont] },

  seq = fun first second => 'Bind { action = first, cont = fun _ => second },

  map = fun f action => 'Bind { include action, cont = fun x => 'Return (f x) },

  run_builder = fun name tag config inputs =>
    'RunBuilder {
      include [name, tag, config, inputs],
    },

  text = fun name config =>
    'RunBuilder {
      include [name, config],
      tag = "Text",
      inputs = {},
    },

  fetch = fun name config =>
    'RunBuilder {
      include [name, config],
      tag = "Fetch",
      inputs = {},
    },

  container_image = fun name config =>
    'RunBuilder {
      include [name, config],
      tag = "ContainerImage",
      inputs = {},
    },

  binary = fun name config image script sources =>
    'RunBuilder {
      include [name, config],
      tag = "Binary",
      inputs = {
        include [image, script, sources],
      },
    },

  image = fun name config base inputs_ =>
    'RunBuilder {
      include [name, config],
      tag = "Image",
      inputs = {
        include base,
        inputs = inputs_,
      },
    },

  sequence = fun actions =>
    std.array.fold_right
      (fun action acc =>
        'Bind {
          action = action,
          cont = fun _ => acc,
        })
      ('Return null)
      actions,
}
