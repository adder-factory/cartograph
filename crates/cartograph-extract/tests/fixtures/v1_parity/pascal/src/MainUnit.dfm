object MainForm: TMainForm
  Left = 0
  Top = 0
  Caption = 'Shop'
  OnCreate = FormCreate
  Items.Strings = (
    'a'
    'b')
  object Panel1: TPanel
    Align = alTop
    object Button1: TButton
      Caption = 'Login'
      OnClick = Button1Click
    end
  end
  inherited Header: THeaderFrame
    OnResize = FormCreate
  end
  inline Footer: TFooterFrame
    Collection = <
      item
        Name = 'x'
      end>
  end
end
